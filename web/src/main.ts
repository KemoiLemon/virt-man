import { openCropDialog } from "./crop";
import { api, cacheFirst, uploadWithProgress } from "./api";
import type { Connection, CreateConnectionResult, DomainSummary, FirewallForward, FirewallPolicy, FirewallRule, NetworkSummary, PoolSummary, StagedUpload, User, VolumeSummary } from "./api";
import { adminRowMenu, adminTitle, adminTree, assignOwner, limitSpecs, renderAdminSection, type AdminCtx } from "./admin";
import { attachConsole, detachConsole } from "./console";
import { closeTayne, openTayne, tayneOpen, type TayneCommand } from "./tayne";
import DOMPurify from "dompurify";
import { closeSvg, ic } from "./icons";
import { skeleton } from "./skeleton";
import { enterLayer, holdFocus } from "./focus";
import { attachLongPress, isCoarsePointer } from "./touch";
import { ADMIN_TABS, parsePath, routeToPath, type AdminSection, type AdminTab, type Route } from "./route";
import { HW_CATALOG, deviceSummary, deviceXml, devicesFromXml, hwType, validateDevice, withDevices } from "./hardware";
import type { HwDevice } from "./hardware";
import { enhanceSelects } from "./listbox";
import { addDiskSample, addHostSample, addNetSample, addSample, avgRateKib, fmtBytes, fmtRate, pct, sparkline } from "./stats";
import type { Timing } from "./stats";
import { bindChartHover, isLive, loadHistory, range, rangeBar, setRange } from "./history";
import { DIR_LABEL, GROUP_LABEL, KIND_LABEL, PAGE, defaultFilter, drillDown, fmtWhen, forLive, liveApplies, logQuery, sortMark, toggleSort, withGroup } from "./access";
import type { FlowDir, Group, Kind, LogFilter } from "./access";
import type { DomainStats, HostStats } from "./stats";
import { runTask } from "./task";
import { closeContextMenu, contextMenuOpen, openContextMenu, type CtxItem } from "./ctxmenu";
import { attachSuggest } from "./suggest";
import { autoVolumeName, domainXml, esc, networkFormFromXml, networkUuid, networkXml, nextFreeIp, poolXml, volumeNameFromFile, volumeXml, withFirmware } from "./xml";
import type { DomainForm, NetworkForm, PoolForm } from "./xml";

/// Title-case a status/type token for display. The API keeps lowercase values
/// (the frontend compares against them), so capitalizing happens only here.
function capLabel(s: string): string {
  if (!s) return s;
  const pretty: Record<string, string> = {
    nostate: "No state",
    pmsuspended: "Suspended",
    shutdown: "Shutdown",
  };
  return pretty[s] ?? s[0].toUpperCase() + s.slice(1);
}

/// Recent-first timestamp for the activity log: relative while it is recent,
/// a clock time for today, and a dated time for anything older. Absolute
/// timestamps on every row would be noise; "just now" is what matters for an
/// action you just ran.
function relTime(ms: number): string {
  const now = Date.now();
  const delta = now - ms;
  if (delta < 0) return "just now"; // clock skew between host and browser
  if (delta < 45_000) return "just now";
  if (delta < 3_600_000) return `${Math.max(1, Math.round(delta / 60_000))}m ago`;
  const then = new Date(ms);
  const time = then.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  // Same calendar day reads as a clock time; anything older needs the date.
  return then.toDateString() === new Date(now).toDateString()
    ? time
    : `${then.toLocaleDateString([], { month: "short", day: "numeric" })} ${time}`;
}

/// Single HTML injection point. Every template's dynamic parts are escaped via
/// esc() at the call site; DOMPurify is the sanitizing backstop the XSS rule
/// requires.
function setHtml(el: Element, html: string) {
  // pi-lens-ignore: no-inner-html, no-inner-html-js
  el.innerHTML = DOMPurify.sanitize(html);
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

/// What an empty list shows: a line saying so, and a small button to create
/// the first one. `id` names the button so the caller can bind it.
function emptyState(what: string, label: string, id: string): string {
  return `<div class="empty empty-create">No ${what} available.
    <button class="btn" id="${id}">${ic.plus(14)} ${label}</button></div>`;
}

// ---------------------------------------------------------------- state

let user: User | null = null;
let authDisabled = false;
let connections: Connection[] = [];
let selectedConn: string | null = null;
let connTab: "dashboard" | "vms" | "networks" | "storage" | "firewall" = "dashboard";
/// Pool whose volume table is expanded; survives the 1s pool poll so the
/// table is re-rendered in place instead of vanishing every second.
let volumesOpen: string | null = null;
/// Pool names last rendered, so the poll can diff and skip a full rebuild.
// null = nothing painted yet. An empty array would mean "painted an empty
// list", and the two used to be the same value: a server with no pools
// compared equal to the fresh container and stayed on its skeleton forever.
let renderedPools: string[] | null = null;
let selectedVm: string | null = null;
let vmTab: "summary" | "hardware" | "options" | "network" | "console" | "xml" = "summary";
/// Which disk the summary's Disks card is graphing, per VM, so the 1s poll
/// does not reset the user's selection back to the first disk.
let summaryDiskSel: Record<string, string> = {};
/// Which NIC the summary's Network card is graphing, per VM.
let summaryNetSel: Record<string, string> = {};
let dcTab: "dashboard" | "guests" = "dashboard";
/// The admin page's open section, or null when it is not the view. A view of
/// its own with its own sidebar, rather than a modal, so it has a URL and
/// works as a page on a phone.
let adminSection: AdminSection | null = null;
/// The open tab of a tabbed admin section (the dashboard), part of the URL.
let adminTab: AdminTab | undefined;
/// Where the return button goes: the last view before the admin page.
let lastNonAdminRoute: Route | null = null;
let pollTimer: number | null = null;
/// Retries for a daemon saved while unreachable: re-fetches its cert every
/// second until it is up, then pins it.
let pendingPoll: number | null = null;
/// VMs per connection, so the sidebar tree can nest guests under their server
/// without every render refetching.
const treeVms: Record<string, DomainSummary[]> = {};
/// Networks and storage pools per connection, shown in the sidebar alongside
/// guests so a network or pool is one click away from its management page.
const treeNets: Record<string, NetworkSummary[]> = {};
const treePools: Record<string, PoolSummary[]> = {};
/// The network/pool currently open in the content pane (mutually exclusive
/// with selectedVm, like the server tabs).
let selectedNet: string | null = null;
let selectedPool: string | null = null;
/// Bumped by every full-pane render. A renderer that has to await a request
/// captures this first and bails if it has moved, so a slow response cannot
/// paint over the view the user has since switched to. Without it the 1s poll
/// races the user: an in-flight network fetch resolves after they clicked a
/// VM and replaces it, which is why the page "just came back".
let renderGen = 0;
/// Console view settings, kept across tab switches and reconnects.
let consoleZoom = 0; // 0 = fit to the stage
let consoleResizeObserver: ResizeObserver | null = null;

/// Tear down a live console: its WebSocket and the stage resize observer.
function closeConsole() {
  consoleResizeObserver?.disconnect();
  consoleResizeObserver = null;
  detachConsole();
}

/// Navigate away from whatever is on screen. Every path that changes the
/// selected VM or view goes through here, so a console session can never be
/// left streaming in the background.
function leaveCurrentView() {
  if (vmTab === "console") closeConsole();
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
async function fastLoad(load: () => Promise<unknown>) {
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
function currentRoute(): Route {
  if (adminSection) return { view: "admin", section: adminSection, tab: adminTab };
  if (!selectedConn) return { view: "dc", tab: dcTab };
  if (selectedVm) return { view: "vm", conn: selectedConn, vm: selectedVm, tab: vmTab };
  if (selectedNet) return { view: "net", conn: selectedConn, net: selectedNet };
  if (selectedPool) return { view: "pool", conn: selectedConn, pool: selectedPool };
  return { view: "conn", conn: selectedConn, tab: connTab };
}

/// Put the current view in the address bar, so a reload returns to it. Called
/// by every view renderer, which is the one place all navigation passes
/// through: the tree, cards, tabs, the context menu and the palette included.
/// A new view pushes a history entry (Back works); re-rendering the same view
/// (the 1s poll) does not.
let restoringRoute = false;
function syncUrl() {
  const path = routeToPath(currentRoute(), connections);
  if (location.pathname === path) return;
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
  if (restoringRoute) history.replaceState(null, "", path);
  else history.pushState(null, "", path);
}

// ----------------------------------------------------------- phone shell
// The sidebar is an off-canvas drawer below 820px; mobile.css does the layout
// and this is the only state that drives it.

function toggleDrawer(open: boolean) {
  document.body.classList.toggle("nav-open", open);
}

function closeDrawer() {
  toggleDrawer(false);
}

/// Adopt the view named by the URL. Used at start-up and on Back/Forward.
function applyRoute(r: Route) {
  leaveCurrentView();
  clearResourceSelection();
  // A non-admin who follows an /admin link lands on the datacenter.
  adminSection = r.view === "admin" && canAdmin() ? r.section : null;
  adminTab = r.view === "admin" ? r.tab : undefined;
  selectedConn = r.view === "dc" || r.view === "admin" ? null : r.conn;
  if (r.view === "admin") {
    // Refused (not an admin): the datacenter, rather than a dead page.
    if (!adminSection) dcTab = "dashboard";
    return;
  }
  if (r.view === "dc") dcTab = r.tab;
  else if (r.view === "conn") connTab = r.tab;
  else if (r.view === "vm") { selectedVm = r.vm; vmTab = r.tab; }
  else if (r.view === "net") selectedNet = r.net;
  else selectedPool = r.pool;
}

/// Render whatever the selection state names. The view renderers do the
/// actual work (and sync the URL).
function renderCurrent() {
  renderTree();
  // The renderers call syncUrl synchronously, before their first await, so
  // the flag covers exactly their URL write.
  restoringRoute = true;
  try {
    renderView();
  } finally {
    restoringRoute = false;
  }
}

function renderView() {
  if (adminSection) return renderAdmin();
  if (selectedConn && isOffline(selectedConn)) return renderOffline();
  if (!selectedConn) renderDc();
  else if (selectedVm) void renderVm();
  else if (selectedNet) { showPageSkeleton(); void fastLoad(renderNetPage); }
  else if (selectedPool) { showPageSkeleton(); void renderPoolPage(); }
  else renderConn();
}

/// The open view's server is down: keep its header (so it is clear which
/// server and page this is, and the tree still lines up) and show one offline
/// panel in place of the body.
function renderOffline() {
  if (!selectedConn) return;
  beginRender();
  syncUrl();
  const c = connections.find((x) => x.id === selectedConn);
  const title = selectedVm ?? selectedNet ?? selectedPool ?? c?.name ?? "";
  const content = $("#content");
  setHtml(content, `<div class="dhead"><span class="dhead-ic">${ic.offline(20)}</span><span class="dtitle">${esc(title)}</span>
    ${selectedVm || selectedNet || selectedPool ? `<span class="badge">${esc(c?.name ?? "")}</span>` : ""}</div>
    ${offlinePanel(selectedConn)}`);
}

// Chart range buttons: one delegated handler, since the dashboards re-render
// every second. Only the charts' own views carry the bar, so re-running the
// current view's poll body repaints exactly them.
document.addEventListener("click", (e) => {
  const b = (e.target as Element | null)?.closest?.("[data-range]") as HTMLElement | null;
  if (!b || !setRange(b.dataset.range!)) return;
  if (!selectedConn) void loadDcDashboard();
  else if (selectedVm) void renderVmBody(selectedVm);
  else void loadDashboard();
});
bindChartHover();

window.addEventListener("popstate", () => {
  if (!user) return;
  applyRoute(parsePath(location.pathname, connections));
  renderCurrent();
});

/// Whether the given guest's tab is still what the pane shows. A tab body that
/// awaits must check before painting, or a late answer overwrites the view the
/// user switched to (same rule as `stillCurrent` for whole-pane renders).
function stillCurrentVm(conn: string, vm: string, tab: typeof vmTab): boolean {
  return selectedConn === conn && selectedVm === vm && vmTab === tab && !!$("#vm-body");
}

/// Replace the pane with a placeholder while a resource page's first read is
/// in flight, so a click answers at once instead of leaving the previous page
/// up until the daemon replies.
function showPageSkeleton() {
  const content = $("#content");
  if (content) setHtml(content, skeleton("page"));
}

/// Claim ownership of the content pane. Call at the top of anything that
/// paints it after an await, then check `stillCurrent(gen)` just before the
/// paint.
function beginRender(): number {
  return ++renderGen;
}

function stillCurrent(gen: number): boolean {
  return gen === renderGen;
}

/// Clear whichever resource the content pane is showing. Every path that
/// leaves a resource page must run this: the poll re-renders whichever of
/// selectedVm/selectedNet/selectedPool is still set, so a stale selection
/// resurrects that page about a second after the user navigates away.
function clearResourceSelection() {
  selectedVm = null;
  selectedNet = null;
  selectedPool = null;
}

const $ = (sel: string, root: ParentNode = document): any =>
  root.querySelector(sel);

// ---------------------------------------------------------------- theme

function initTheme() {
  const t = localStorage.getItem("virt-man-theme") ?? "dark";
  document.documentElement.setAttribute("data-theme", t);
}
function toggleTheme() {
  const next = document.documentElement.getAttribute("data-theme") === "dark" ? "light" : "dark";
  document.documentElement.setAttribute("data-theme", next);
  localStorage.setItem("virt-man-theme", next);
}

// ------------------------------------------------------- command palette

/// Build the command list for the current context. Called on every open so the
/// palette always reflects live state (which server is selected, which guests
/// exist) rather than a snapshot taken at boot.
function openCommandPalette() {
  const cmds: TayneCommand[] = [];
  const add = (id: string, label: string, hint: string | undefined, icon: string, run: () => void) =>
    cmds.push({ id, label, hint, icon, run });
  const toDc = (tab: typeof dcTab) => () => {
    leaveCurrentView();
    selectedConn = null;
    clearResourceSelection();
    dcTab = tab;
    renderTree();
    renderDc();
  };

  // Actions on what is open come first: they are what you most likely want.
  if (selectedConn && selectedVm) {
    const conn = selectedConn;
    const vm = selectedVm;
    const running = (treeVms[conn] ?? []).find((v) => v.name === vm)?.active === true;
    const power = (id: string, label: string, icon: string, path: string) =>
      add(id, `${label} — ${vm}`, "This VM", icon, () => void treeVmPower(conn, vm, path));
    if (running) {
      add("vm-vv", `Open in virt-viewer — ${vm}`, "This VM", ic.download(), () => void downloadVv(conn, vm));
      power("vm-shutdown", "Shutdown", ic.stop(), "/shutdown");
      power("vm-reboot", "Reboot", ic.reboot(), "/reboot");
      power("vm-reset", "Reset", ic.reset(), "/reset");
      power("vm-destroy", "Force stop", ic.forceStop(), "/destroy");
    } else {
      power("vm-start", "Start", ic.start(), "/start");
      // libvirt can only rename a shut-off guest.
      add("vm-rename", `Rename — ${vm}`, "This VM", ic.edit(), () => void renameVm(vm));
    }
    add("vm-delete", `Delete — ${vm}`, "This VM", ic.trash(), () => void deleteVm(vm));
  }
  if (selectedConn && selectedNet) {
    const net = selectedNet;
    add("net-edit", `Edit network — ${net}`, "This network", ic.edit(), () => void openEditNetwork(net));
    add("net-fw", `Firewall rules — ${net}`, "This network", ic.firewall(), () => openFirewallEditor(net));
    add("net-delete", `Delete network — ${net}`, "This network", ic.trash(),
      () => void deleteNetwork(net).then(() => refreshTreeVms()));
  }
  if (selectedConn && selectedPool) {
    const pool = selectedPool;
    add("pool-upload", `Upload ISO — ${pool}`, "This pool", ic.upload(), () => openUpload(pool));
    add("pool-volume", `New volume — ${pool}`, "This pool", ic.disk(), () => openCreateVolume(pool));
  }

  // Creation. Resource kinds that need a server are offered for the one open.
  add("add-resource", "Add resource", "VM, network, pool or server", ic.plus(), () => openAddResource());
  add("add-server", "Add server", "Connect to a daemon", ic.server(), () => openConnectionDialog());
  if (selectedConn) {
    const server = connections.find((c) => c.id === selectedConn)?.name ?? "";
    add("create-vm", "Create VM", server, ic.vm(), () => openCreateVm());
    add("create-net", "Create network", server, ic.network(), () => openCreateNetwork());
    add("create-pool", "Create pool", server, ic.storage(), () => openCreatePool());
  }

  // Navigation. Every page with a URL is reachable, and the hint says where
  // it lives, so typing a server, guest, network or pool name finds it.
  add("nav-dc", "Datacenter", "Dashboard", ic.datacenter(), toDc("dashboard"));
  add("nav-dc-guests", "Datacenter — Guests", "/guests", ic.vms(), toDc("guests"));
  const connTabs: [typeof connTab, string, string][] = [
    ["dashboard", "Dashboard", ic.datacenter()],
    ["vms", "VMs", ic.vms()],
    ["networks", "Networks", ic.network()],
    ["storage", "Storage", ic.storage()],
    ["firewall", "Firewall", ic.firewall()],
  ];
  const vmTabs: [typeof vmTab, string, string][] = [
    ["summary", "Summary", ic.vm()],
    ["console", "Console", ic.vm()],
    ["hardware", "Hardware", ic.cpu()],
    ["options", "Options", ic.options()],
    ["network", "Network", ic.network()],
    ["xml", "XML", ic.xml()],
  ];
  for (const c of connections) {
    for (const [tab, label, icon] of connTabs) {
      add(`nav-${c.id}-${tab}`, `${c.name} — ${label}`, "Server", icon, () => selectServer(c.id, tab));
    }
    for (const v of treeVms[c.id] ?? []) {
      const name = v.title || v.name;
      for (const [tab, label, icon] of vmTabs) {
        add(`nav-${c.id}-vm-${v.name}-${tab}`, tab === "summary" ? `VM — ${name}` : `VM — ${name} — ${label}`,
          c.name, icon, () => selectVmRow(c.id, v.name, tab));
      }
    }
    for (const n of treeNets[c.id] ?? []) {
      add(`nav-${c.id}-net-${n.name}`, `Network — ${n.name}`, c.name, ic.network(), () => selectNetRow(c.id, n.name));
    }
    for (const pl of treePools[c.id] ?? []) {
      add(`nav-${c.id}-pool-${pl.name}`, `Pool — ${pl.name}`, c.name, ic.storage(), () => selectPoolRow(c.id, pl.name));
    }
  }

  add(
    "toggle-theme",
    "Toggle theme",
    document.documentElement.getAttribute("data-theme") === "dark" ? "dark → light" : "light → dark",
    ic.theme(),
    toggleTheme,
  );
  if (canAdmin()) {
    add("admin-panel", "Administration", "Users, groups, permissions, resources, limits", ic.admin(), () => openAdmin());
    for (const [sec, label] of [["users", "Users"], ["groups", "Groups"], ["permissions", "Permissions"],
      ["vms", "VMs"], ["networks", "Networks"], ["pools", "Storage Pools"],
      ["quotas", "Quotas"], ["ips", "IP Addresses"], ["daemons", "Daemons"],
      ["activity", "Activity"]] as const) {
      add(`admin-${sec}`, `Administration — ${label}`, `/admin/${sec}`, ic.admin(), () => openAdmin(sec));
    }
  }
  if (!authDisabled) add("logout", "Sign out", "End this session", ic.logout(), () => void logout());

  openTayne(cmds);
}

// ------------------------------------------------------------- global keys

/// Ctrl+K opens the palette; Escape closes the palette, then the topmost
/// modal (dispatched as a synthetic event so askText/askConfirm can resolve
/// their promise). The guest owns the keyboard while the console is focused,
/// so the palette stays out of the way there.
document.addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && (e.key === "k" || e.key === "K")) {
    // Only inside the real app: the login/setup screens have no commands to
    // offer, and the guest owns the keyboard while the console is focused.
    if (!user) return;
    const active = document.activeElement as HTMLElement | null;
    if (active?.closest?.(".console-stage")) return;
    e.preventDefault();
    if (tayneOpen()) closeTayne();
    else openCommandPalette();
    return;
  }
  if (e.key === "Escape") {
    // One press dismisses one layer, topmost first: the context menu sits
    // above the palette, which sits above any modal.
    if (contextMenuOpen()) {
      closeContextMenu();
      return;
    }
    if (tayneOpen()) {
      closeTayne();
      return;
    }
    const modals = document.querySelectorAll(".modal-back.open");
    const top = modals[modals.length - 1];
    if (top) top.dispatchEvent(new Event("modal-escape", { bubbles: false }));
  }
});

// ---------------------------------------------------------------- toast
// Toasts stack bottom-right in a column instead of overlapping at the
// bottom-centre, and each one is dismissible.
function toast(msg: string, kind: "ok" | "bad" = "ok") {
  let dock = document.getElementById("toast-dock");
  if (!dock) {
    dock = document.createElement("div");
    dock.id = "toast-dock";
    document.body.appendChild(dock);
  }
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  const text = document.createElement("span");
  text.className = "toast-msg";
  text.textContent = msg;
  const close = document.createElement("button");
  close.className = "toast-close";
  close.type = "button";
  close.setAttribute("aria-label", "Dismiss");
  close.append(closeSvg());
  el.append(text, close);
  dock.appendChild(el);
  const dismiss = () => {
    el.classList.remove("show");
    setTimeout(() => el.remove(), 200);
  };
  close.onclick = dismiss;
  requestAnimationFrame(() => el.classList.add("show"));
  // A failure stays until dismissed: it carries the reason (which limit, which
  // permission), and four seconds is not long enough to read one -- the
  // refusals were being missed entirely. Successes still fade on their own.
  if (kind !== "bad") setTimeout(dismiss, 4000);
}

// ---------------------------------------------------------------- boot

async function boot() {
  initTheme();
  const status = await api.get<{ needs_setup: boolean; auth_disabled?: boolean }>("/auth/status");
  if (status.auth_disabled) {
    authDisabled = true;
    user = { id: "local", username: "local", nickname: "local", avatar: null, admin: true, privs: [] };
    await loadConnections();
    applyRoute(parsePath(location.pathname, connections));
    renderApp();
    return;
  }
  if (status.needs_setup) return renderSetup();
  try {
    const me = await api.get<User>("/auth/me");
    user = me;
    await loadConnections();
    applyRoute(parsePath(location.pathname, connections));
    renderApp();
  } catch {
    renderLogin();
  }
}

/// Fetch the saved daemon connections. Without this the sidebar tree and the
/// Datacenter table render empty on every load until one is added or removed.
async function loadConnections() {
  try {
    connections = await api.get<Connection[]>("/connections");
  } catch {
    connections = [];
  }
}

// ---------------------------------------------------------------- auth screens

function centerScreen(html: string) {
  const app = $("#app");
setHtml(app, `<div class="center-screen">${html}</div>`);
}

function renderSetup() {
  centerScreen(`
    <div class="auth-box">
      <div class="auth-logo">virt-<span>man</span></div>
      <div class="modal-body">
        <label class="field"><span>Username</span><input id="su" autocomplete="username"></label>
        <label class="field"><span>Password (min 8 chars)</span><input id="sp" type="password" autocomplete="new-password"></label>
        <label class="field"><span>Nickname</span><input id="sn"></label>
      </div>
      <div class="modal-foot"><button class="btn primary" id="setup-btn">Create Admin</button></div>
    </div>`);
  $("#setup-btn").onclick = async () => {
    try {
      await api.post("/auth/setup", {
        username: $("#su").value,
        password: $("#sp").value,
        nickname: $("#sn").value,
      });
      toast("Admin created");
      boot();
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };
}

function renderLogin() {
  centerScreen(`
    <div class="auth-box">
      <div class="auth-logo">virt-<span>man</span></div>
      <div class="modal-body">
        <label class="field"><span>Username</span><input id="lu" autocomplete="username"></label>
        <label class="field"><span>Password</span><input id="lp" type="password" autocomplete="current-password"></label>
      </div>
      <div class="modal-foot"><button class="btn primary" id="login-btn">Sign In</button></div>
    </div>`);
  $("#login-btn").onclick = async () => {
    try {
      user = await api.post<User>("/auth/login", { username: $("#lu").value, password: $("#lp").value });
      // boot() loads connections before rendering; the login path must too, or
      // the sidebar tree and Datacenter table render empty until a refresh.
      await loadConnections();
      // Land on the page they asked for before being sent to sign in.
      applyRoute(parsePath(location.pathname, connections));
      renderApp();
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };
  $("#lp").onkeydown = (e: KeyboardEvent) => { if (e.key === "Enter") $("#login-btn").click(); };
}

async function logout() {
  stopPendingPoll();
  await api.post("/auth/logout");
  user = null;
  renderLogin();
}

// ---------------------------------------------------------------- app shell

function renderApp() {
  if (!user) return;
  const app = $("#app");
setHtml(app, `
    <div class="topbar">
      <button class="btn icon mobile-only" id="nav-btn" title="Servers and guests">${ic.menu()}</button>
      <span class="brand">virt-<span>man</span></span>
      <span class="host">web${authDisabled ? " · local" : ` · ${esc(user.nickname)}`}</span>
      <span class="topbar-quick">
        <button class="btn icon mobile-only" id="palette-btn" title="Commands">${ic.logs()}</button>
        <button class="btn icon" id="theme-btn" title="Toggle theme">${ic.theme()}</button>
        ${canAdmin() ? `<button class="btn icon" id="admin-btn"></button>` : ""}
        ${authDisabled ? "" : `<button class="btn icon" id="logout-btn" title="Sign out">${ic.logout()}</button>`}
      </span>
    </div>
    <div class="layout">
      <div class="pane list-pane">
        <div class="restree" id="tree"></div>
        ${authDisabled ? "" : `<div class="side-profile" id="profile-btn"></div>`}
      </div>
      <div class="pane">
        <div class="detail" id="content"></div>
      </div>
    </div>
    <div class="nav-scrim" id="nav-scrim"></div>`);

  closeDrawer();
  $("#nav-btn").onclick = () => toggleDrawer(true);
  $("#nav-scrim").onclick = closeDrawer;
  $("#palette-btn").onclick = openCommandPalette;
  $("#theme-btn").onclick = toggleTheme;
  if ($("#logout-btn")) $("#logout-btn").onclick = logout;
  if ($("#admin-btn")) $("#admin-btn").onclick = () => (adminSection ? leaveAdmin() : openAdmin());

  renderProfileBtn();
  renderCurrent();
  // Populate the auto-expanded tree immediately rather than waiting for the
  // first poll tick or a server click.
  void fastLoad(refreshTreeVms);
  startPolling();
  // Pick up a transfer that was already running before this page loaded, so a
  // reload mid-upload still shows its progress instead of looking stalled.
  void pollUploads();
}

/// Whether to offer the admin page: `Permissions.Modify` at `/`, as the
/// server reports it in /auth/me. Hiding the button is cosmetic -- the API
/// refuses every admin call from anyone else.
function canAdmin(): boolean {
  return !!user?.admin;
}

// ---------------------------------------------------------------- downtime

/// Whether the web server's probes currently reach this server's daemon.
function isOffline(connId: string | null): boolean {
  return !!connId && connections.find((c) => c.id === connId)?.online === false;
}

/// "3 min", "2 h", for how long something has been down.
function ago(ms: number): string {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return `${s} s`;
  if (s < 3600) return `${Math.round(s / 60)} min`;
  if (s < 86400) return `${Math.round(s / 3600)} h`;
  return `${Math.round(s / 86400)} d`;
}

/// One panel for a server that cannot be reached, in place of the raw
/// transport error each view used to print ("daemon error: error sending
/// request … Connection refused").
function offlinePanel(connId: string): string {
  const c = connections.find((x) => x.id === connId);
  return `<div class="offline-panel" data-offline="${esc(connId)}">
    <span class="offline-ic">${ic.offline(28)}</span>
    <div>
      <b>${esc(c?.name ?? "This server")} is offline</b>
      <p>${esc(c?.offline_error ?? "The daemon is not answering.")}${c?.offline_since_ms ? ` · for ${ago(c.offline_since_ms)}` : ""}</p>
      <p class="muted">Retrying every few seconds; this page reloads by itself when it is back.</p>
    </div>
  </div>`;
}

/// Refresh reachability. When a server's state flips, repaint what shows it:
/// the sidebar always, and the open view if it belongs to that server (a view
/// that came back re-reads everything, instead of waiting for the user).
async function refreshHealth() {
  let fresh: Connection[];
  try {
    fresh = await api.get<Connection[]>("/connections");
  } catch {
    return; // the web server itself is unreachable; nothing to compare
  }
  const was = new Map(connections.map((c) => [c.id, c.online !== false] as const));
  const flipped = fresh.filter((c) => was.has(c.id) && was.get(c.id) !== (c.online !== false));
  connections = fresh;
  if (!flipped.length) return;
  renderTree();
  for (const c of flipped) {
    toast(c.online === false ? `${c.name} went offline` : `${c.name} is back online`, c.online === false ? "bad" : "ok");
  }
  if (!selectedConn || flipped.some((c) => c.id === selectedConn)) {
    if (flipped.some((c) => c.online !== false)) void refreshTreeVms();
    renderView();
  }
}

// ---------------------------------------------------------------- tree

/// Nested resource tree: Datacenter root -> servers -> guests, mirroring
/// vm-man's sidebar. Guests come from the per-connection cache, so opening a
/// server reveals its VMs inline instead of only in the content pane.
function renderTree() {
  const tree = $("#tree");
  if (!tree) return;
  syncAdminButton();
  if (adminSection) {
    const restore = holdFocus(tree, ".tree-row.sel");
    setHtml(tree, adminTree(adminSection));
    restore();
    tree.querySelectorAll("[data-admin-section]").forEach((row: Element) => {
      (row as HTMLElement).onclick = () => openAdmin((row as HTMLElement).dataset.adminSection as AdminSection);
    });
    return;
  }

  const serverRows = connections
    .map((c) => {
      const selected = c.id === selectedConn && !selectedVm && !selectedNet && !selectedPool;
      const vms = treeVms[c.id] ?? [];
      const nets = treeNets[c.id] ?? [];
      const pools = treePools[c.id] ?? [];
      const running = vms.filter((v) => v.active).length;
      const offline = c.online === false;
      // Servers auto-expand: guests show as soon as they are known, without
      // requiring the server to be selected first.
      // A guest row carries its owning connection: selecting it must switch
      // servers too, otherwise the VM view has no connection to read from.
      const guestRows =
        vms.length
          ? vms
              .map(
                (v) => `
      <button class="tree-row tree-vm ${selectedVm === v.name && selectedConn === c.id ? "sel" : ""}"
        data-vm="${esc(v.name)}" data-vm-conn="${c.id}">
        <span class="tree-ic ${v.active ? "on" : ""}">${ic.vm()}</span>
        <span class="tree-label">${esc(v.title || v.name)}</span>
        <span class="tree-state ${v.active ? "on" : ""}">${esc(capLabel(v.state))}</span>
      </button>`,
              )
              .join("")
          : "";
      const netRows = nets.length
        ? nets
            .map(
              (n) => `
      <button class="tree-row tree-res tree-net ${selectedNet === n.name && selectedConn === c.id ? "sel" : ""}"
        data-net="${esc(n.name)}" data-net-conn="${c.id}">
        <span class="tree-ic ${n.active ? "on" : ""}">${ic.network()}</span>
        <span class="tree-label">${esc(n.name)}</span>
        <span class="tree-state ${n.active ? "on" : ""}">${n.active ? "Active" : "Inactive"}</span>
      </button>`,
            )
            .join("")
        : "";
      const poolRows = pools.length
        ? pools
            .map(
              (p) => `
      <button class="tree-row tree-res tree-pool ${selectedPool === p.name && selectedConn === c.id ? "sel" : ""}"
        data-pool="${esc(p.name)}" data-pool-conn="${c.id}">
        <span class="tree-ic ${p.active ? "on" : ""}">${ic.storage()}</span>
        <span class="tree-label">${esc(p.name)}</span>
        <span class="tree-state ${p.active ? "on" : ""}">${p.active ? "Active" : "Inactive"}</span>
      </button>`,
            )
            .join("")
        : "";
      // Offline: the guest, network and pool states below are last known,
      // not current, so they are greyed rather than shown as live.
      return `
      <button class="tree-row tree-group ${selected ? "sel" : ""} ${offline ? "offline" : ""}" data-conn="${c.id}"
        ${offline ? `title="Offline${c.offline_error ? `: ${esc(c.offline_error)}` : ""}"` : ""}>
        <span class="tree-ic ${running > 0 && !offline ? "on" : ""}">${offline ? ic.offline() : ic.server()}</span>
        <span class="tree-label">${esc(c.name)}</span>
        <span class="tree-state ${offline ? "bad" : ""}">${offline ? "Offline" : vms.length ? `${running}/${vms.length}` : ""}</span>
      </button>${offline ? `<div class="tree-stale">${guestRows}${netRows}${poolRows}</div>` : `${guestRows}${netRows}${poolRows}`}`;
    })
    .join("");

  const restoreTreeFocus = holdFocus(tree, ".tree-row.sel");
  setHtml(tree, `
    <button class="tree-row tree-root ${selectedConn === null ? "sel" : ""}" id="dc-row">
      <span class="tree-ic">${ic.datacenter()}</span>
      <span class="tree-label">Datacenter</span>
    </button>
    ${serverRows || `<div class="tree-empty">No servers yet — add one below.</div>`}`);

  restoreTreeFocus();
  $("#dc-row").onclick = () => {
    leaveCurrentView();
    selectedConn = null;
    clearResourceSelection();
    dcTab = "dashboard";
    renderTree();
    renderDc();
  };
  tree.querySelectorAll("[data-conn]").forEach((row: Element) => {
    (row as HTMLElement).onclick = () => {
      leaveCurrentView();
      selectedConn = (row as HTMLElement).dataset.conn!;
      clearResourceSelection();
      connTab = "dashboard";
      renderTree();
      renderConn();
      void refreshTreeVms();
    };
  });
  tree.querySelectorAll("[data-vm]").forEach((row: Element) => {
    const el = row as HTMLElement;
    el.onclick = () => {
      // Set the connection first: renderVm() bails out when selectedConn is
      // null, which made guest clicks silently do nothing from the
      // Datacenter view or a different server's branch.
      leaveCurrentView();
      selectedConn = el.dataset.vmConn!;
      clearResourceSelection();
      selectedVm = el.dataset.vm!;
      vmTab = "summary";
      renderTree();
      void renderVm();
    };
  });
  tree.querySelectorAll("[data-net]").forEach((row: Element) => {
    const el = row as HTMLElement;
    el.onclick = () => {
      leaveCurrentView();
      selectedConn = el.dataset.netConn!;
      clearResourceSelection();
      selectedNet = el.dataset.net!;
      renderTree();
      showPageSkeleton();
      void fastLoad(renderNetPage);
    };
  });
  tree.querySelectorAll("[data-pool]").forEach((row: Element) => {
    const el = row as HTMLElement;
    el.onclick = () => {
      leaveCurrentView();
      selectedConn = el.dataset.poolConn!;
      clearResourceSelection();
      selectedPool = el.dataset.pool!;
      renderTree();
      showPageSkeleton();
      void renderPoolPage();
    };
  });
}

// ------------------------------------------------------------- right-click

/// Select a server (optionally opening one of its tabs).
function selectServer(id: string, tab?: typeof connTab) {
  leaveCurrentView();
  selectedConn = id;
  selectedVm = null;
  selectedNet = null;
  selectedPool = null;
  if (tab) connTab = tab;
  renderTree();
  renderConn();
}

/// Select a guest (optionally opening one of its tabs).
function selectVmRow(conn: string, vm: string, tab?: typeof vmTab) {
  leaveCurrentView();
  selectedConn = conn;
  selectedVm = vm;
  selectedNet = null;
  selectedPool = null;
  if (tab) vmTab = tab;
  renderTree();
  void renderVm();
}

function selectNetRow(conn: string, net: string) {
  leaveCurrentView();
  selectedConn = conn;
  selectedVm = null;
  selectedNet = net;
  selectedPool = null;
  renderTree();
  showPageSkeleton();
  void fastLoad(renderNetPage);
}

function selectPoolRow(conn: string, pool: string) {
  leaveCurrentView();
  selectedConn = conn;
  selectedVm = null;
  selectedNet = null;
  selectedPool = pool;
  renderTree();
  showPageSkeleton();
  void renderPoolPage();
}

/// Run a lifecycle POST for a guest straight from the tree, without needing
/// that guest to be open: refresh the sidebar, and repaint the VM view when
/// the action was aimed at the guest already on screen.
async function treeVmPower(conn: string, vm: string, path: string) {
  try {
    await act(`/proxy/${conn}/domains/${encodeURIComponent(vm)}${path}`, "POST");
  } catch {
    return; // act already toasted the daemon's reason
  }
  await refreshTreeVms();
  if (selectedConn === conn && selectedVm === vm) void renderVm();
}

async function treeNetPower(conn: string, net: string, action: "start" | "stop") {
  try {
    await act(`/proxy/${conn}/networks/${encodeURIComponent(net)}/${action}`, "POST");
  } catch {
    return;
  }
  await refreshTreeVms();
  if (selectedConn === conn && selectedNet === net) void renderNetPage();
}

/// Actions for one guest. Shared by the tree, the VM cards and the datacenter
/// guest cards, so a guest offers the same menu wherever it is right-clicked.
/// Download a virt-viewer file for a running guest and hand it to the
/// browser. Fetched on click, never linked: the file names a relay the daemon
/// opens for one viewer and one minute, so a stale href would point nowhere.
async function downloadVv(connId: string, vm: string) {
  try {
    const res = await fetch(`/proxy/${connId}/domains/${encodeURIComponent(vm)}/console/vv`, { credentials: "same-origin" });
    if (!res.ok) {
      const e = await res.json().catch(() => ({ error: res.statusText }));
      throw new Error((e as { error?: string }).error ?? res.statusText);
    }
    const url = URL.createObjectURL(await res.blob());
    const a = document.createElement("a");
    a.href = url;
    a.download = `${vm}.vv`;
    document.body.append(a);
    a.click();
    a.remove();
    window.setTimeout(() => URL.revokeObjectURL(url), 10_000);
    toast("virt-viewer file downloaded — open it within a minute");
  } catch (e) {
    toast(`virt-viewer: ${(e as Error).message}`, "bad");
  }
}

/// Whether the signed-in user may share this guest: its owner or an admin.
/// Cosmetic, like `canAdmin`; the server checks the same rule.
function canShareVm(connId: string, vmName: string): boolean {
  const guest = (treeVms[connId] ?? []).find((v) => v.name === vmName);
  return canAdmin() || (!!guest?.mine && !guest?.suspended);
}

/// Share one of your guests with other users, at a fixed level.
async function openShareVm(connId: string, vmName: string) {
  let data: { shares: { user: string; username: string; role: string }[]; roles: string[] };
  let people: { id: string; username: string }[];
  try {
    const q = `conn=${encodeURIComponent(connId)}&vm=${encodeURIComponent(vmName)}`;
    [data, people] = await Promise.all([
      api.get<typeof data>(`/shares?${q}`),
      api.get<typeof people>("/shares/users"),
    ]);
  } catch (e) {
    return toast((e as Error).message, "bad");
  }
  const LEVEL: Record<string, string> = {
    VMUser: "Use — console, start and stop",
    VMAdmin: "Manage — also change its hardware and settings",
  };
  const el = openModal(
    `Share — ${esc(vmName)}`,
    `<p class="hint">People you share with see this VM in their list. They cannot delete it or share it onward.</p>
     <div class="section-title">Shared with</div>
     <table class="grid" id="sh-list">
       <tr><th>User</th><th>Access</th><th class="col-actions"></th></tr>
       ${data.shares.map((x) => `<tr><td class="tname">${esc(x.username)}</td><td>${esc(LEVEL[x.role] ?? x.role)}</td>
         <td class="col-actions"><button class="btn danger icon" data-unshare="${esc(x.user)}" title="Stop sharing">${ic.trash()}</button></td></tr>`).join("")
         || `<tr><td colspan="3" class="empty">Not shared with anyone.</td></tr>`}
     </table>
     <div class="section-title">Add someone</div>
     <div class="grid2">
       <label class="field"><span>User</span><select id="sh-user">
         ${people.map((u) => `<option value="${esc(u.id)}">${esc(u.username)}</option>`).join("")}
       </select></label>
       <label class="field"><span>Access</span><select id="sh-role">
         ${data.roles.map((r) => `<option value="${esc(r)}">${esc(LEVEL[r] ?? r)}</option>`).join("")}
       </select></label>
     </div>`,
    `<button class="btn" id="sh-close">Close</button><button class="btn primary" id="sh-add">${ic.plus(14)} Share</button>`,
  );
  const body = { conn: connId, vm: vmName };
  $("#sh-close", el).onclick = () => el.remove();
  $("#sh-add", el).onclick = async () => {
    try {
      await api.put("/shares", { ...body, user: $("#sh-user", el).value, role: $("#sh-role", el).value });
      toast(`${vmName} shared`);
      el.remove();
      void openShareVm(connId, vmName);
    } catch (e) { toast((e as Error).message, "bad"); }
  };
  el.querySelectorAll<HTMLElement>("[data-unshare]").forEach((b) => {
    b.onclick = async () => {
      try {
        await api.del("/shares", { ...body, user: b.dataset.unshare });
        toast("Sharing stopped");
        el.remove();
        void openShareVm(connId, vmName);
      } catch (e) { toast((e as Error).message, "bad"); }
    };
  });
}

function vmMenu(connId: string, vmName: string): CtxItem[] {
  const guest = (treeVms[connId] ?? []).find((v) => v.name === vmName);
  const running = guest?.active === true;
  return [
    { label: "Open", icon: ic.vm(), run: () => selectVmRow(connId, vmName, "summary") },
    {
      label: "Start", icon: ic.start(), disabled: running, separator: true,
      run: () => void treeVmPower(connId, vmName, "/start"),
    },
    {
      label: "Shutdown", icon: ic.stop(), disabled: !running,
      run: () => void treeVmPower(connId, vmName, "/shutdown"),
    },
    {
      label: "Reboot", icon: ic.reboot(), disabled: !running,
      run: () => void treeVmPower(connId, vmName, "/reboot"),
    },
    {
      label: "Reset", icon: ic.reset(), disabled: !running,
      run: () => void treeVmPower(connId, vmName, "/reset"),
    },
    {
      label: "Force stop", icon: ic.forceStop(), disabled: !running,
      run: () => void treeVmPower(connId, vmName, "/destroy"),
    },
    { label: "Console", icon: ic.vm(), separator: true, run: () => selectVmRow(connId, vmName, "console") },
    { label: "Open in virt-viewer", icon: ic.download(), disabled: !running, run: () => void downloadVv(connId, vmName) },
    { label: "Hardware", icon: ic.cpu(), run: () => selectVmRow(connId, vmName, "hardware") },
    { label: "Options", icon: ic.options(), run: () => selectVmRow(connId, vmName, "options") },
    { label: "Network", icon: ic.network(), run: () => selectVmRow(connId, vmName, "network") },
    { label: "XML", icon: ic.xml(), run: () => selectVmRow(connId, vmName, "xml") },
    ...(canShareVm(connId, vmName)
      ? [{ label: "Share", icon: ic.perms(), separator: true, run: () => void openShareVm(connId, vmName) }]
      : []),
    {
      // Renaming is libvirt-only for shut-off guests; offer it only then.
      label: "Rename", icon: ic.edit(), separator: !canShareVm(connId, vmName), disabled: running,
      run: () => { selectedConn = connId; void renameVm(vmName); },
    },
    {
      label: "Delete", icon: ic.trash(), danger: true,
      run: () => {
        selectVmRow(connId, vmName);
        void deleteVm(vmName).then(() => refreshTreeVms());
      },
    },
  ];
}

/// Actions for one network (tree row, network card, dashboard row).
function netMenu(connId: string, netName: string): CtxItem[] {
  const net = (treeNets[connId] ?? []).find((n) => n.name === netName);
  const active = net?.active === true;
  const autostart = net?.autostart === true;
  // The editors read the selected server; point it at this network's owner
  // first, since a dashboard row can belong to a server that is not selected.
  const onConn = (f: () => void) => () => { selectedConn = connId; f(); };
  return [
    { label: "Open", icon: ic.network(), run: () => selectNetRow(connId, netName) },
    {
      label: active ? "Stop" : "Start", icon: active ? ic.stop() : ic.start(), separator: true,
      run: () => void treeNetPower(connId, netName, active ? "stop" : "start"),
    },
    {
      label: autostart ? "Disable autostart" : "Enable autostart", icon: ic.autostart(),
      run: async () => {
        try {
          await api.post(`/proxy/${connId}/networks/${encodeURIComponent(netName)}/autostart`,
            { autostart: !autostart });
          toast("OK");
        } catch (e) {
          return toast((e as Error).message, "bad");
        }
        await refreshTreeVms();
      },
    },
    { label: "Edit network", icon: ic.edit(), separator: true, run: onConn(() => void openEditNetwork(netName)) },
    { label: "Firewall rules", icon: ic.firewall(), run: onConn(() => openFirewallEditor(netName)) },
    {
      label: "Delete", icon: ic.trash(), danger: true, separator: true,
      run: () => {
        selectNetRow(connId, netName);
        void deleteNetwork(netName).then(() => refreshTreeVms());
      },
    },
  ];
}

/// Actions for one storage pool (tree row, pool card, dashboard card).
function poolMenu(connId: string, poolName: string): CtxItem[] {
  const onConn = (f: () => void) => () => { selectedConn = connId; f(); };
  return [
    { label: "Open", icon: ic.storage(), run: () => selectPoolRow(connId, poolName) },
    { label: "Upload ISO", icon: ic.upload(), separator: true, run: onConn(() => openUpload(poolName)) },
    { label: "Download ISO from URL", icon: ic.download(), run: onConn(() => openFetch(poolName)) },
    { label: "New volume", icon: ic.disk(), run: onConn(() => openCreateVolume(poolName)) },
    {
      label: "Delete pool", icon: ic.trash(), danger: true, separator: true,
      run: () => {
        selectPoolRow(connId, poolName);
        void deletePool(poolName).then(() => refreshTreeVms());
      },
    },
  ];
}

/// Actions for one server (tree row, server card, dashboard row).
function serverMenu(serverId: string): CtxItem[] {
  const create = (f: () => void) => () => { selectedConn = serverId; renderTree(); f(); };
  return [
    { label: "Dashboard", icon: ic.datacenter(), run: () => selectServer(serverId, "dashboard") },
    { label: "VMs", icon: ic.vms(), run: () => selectServer(serverId, "vms") },
    { label: "Networks", icon: ic.network(), run: () => selectServer(serverId, "networks") },
    { label: "Storage", icon: ic.storage(), run: () => selectServer(serverId, "storage") },
    { label: "Firewall", icon: ic.firewall(), run: () => selectServer(serverId, "firewall") },
    { label: "Create VM", icon: ic.plus(), separator: true, run: create(() => openCreateVm()) },
    { label: "Create network", icon: ic.plus(), run: create(() => openCreateNetwork()) },
    { label: "Create pool", icon: ic.plus(), run: create(() => openCreatePool()) },
    { label: "Edit server", icon: ic.edit(), separator: true, run: () => openConnectionDialog(serverId) },
    { label: "Remove server", icon: ic.trash(), danger: true, run: () => void deleteConnection(serverId) },
  ];
}

/// Creation and refresh: the sidebar background and the Datacenter root.
/// This is where Add Resource lives now that the sidebar button is gone.
function rootMenu(): CtxItem[] {
  return [
    { label: "Add Resource", icon: ic.plus(), run: () => openAddResource() },
    { label: "Add server", icon: ic.server(), run: () => openConnectionDialog() },
    { label: "Refresh", icon: ic.reboot(), separator: true, run: () => void refreshTreeVms() },
  ];
}

/// The actions offered for whatever tree row was right-clicked.
function treeMenuItems(row: HTMLElement | null): CtxItem[] {
  if (!row || row.id === "dc-row") return rootMenu();
  const d = row.dataset;
  if (d.vm && d.vmConn) return vmMenu(d.vmConn, d.vm);
  if (d.net && d.netConn) return netMenu(d.netConn, d.net);
  if (d.pool && d.poolConn) return poolMenu(d.poolConn, d.pool);
  if (d.conn) return serverMenu(d.conn);
  return rootMenu();
}

/// A right-clicked admin sidebar row: open it, and its section's main action.
/// Without this the admin sidebar offered the resource tree's menu (Add
/// server, Create VM), which belongs to a page the user has left.
function adminTreeMenu(row: HTMLElement | null): CtxItem[] | null {
  const sec = row?.dataset.adminSection as AdminSection | undefined;
  if (!sec) return null;
  const open = (s: AdminSection) => () => openAdmin(s);
  // What each section's own add button does, reached from the menu. It opens
  // the section, then clicks its real button, so it is the same code path.
  const add: Partial<Record<AdminSection, [string, string]>> = {
    users: ["Add user", "#add-user"],
    groups: ["Add group", "#add-group"],
    roles: ["New role", "#add-role"],
    permissions: ["Add permission", "#add-acl"],
    daemons: ["Add daemon", "#add-daemon"],
    keys: ["New API key", "#add-key"],
    vms: ["Create VM for a user", "#res-new"],
    networks: ["Create network for a user", "#res-new"],
  };
  const items: CtxItem[] = [{ label: `Open ${row!.querySelector(".tree-label")?.textContent?.trim() ?? ""}`, icon: ic.logs(), run: open(sec) }];
  const a = add[sec];
  if (a) {
    items.push({
      label: a[0], icon: ic.plus(), separator: true,
      run: () => {
        openAdmin(sec);
        // The section paints after an await; click once its button is there.
        const tryClick = (n: number) => {
          const b = document.querySelector<HTMLElement>(`#admin-area ${a[1]}`);
          if (b) b.click();
          else if (n > 0) setTimeout(() => tryClick(n - 1), 100);
        };
        tryClick(50);
      },
    });
  }
  items.push({ label: "Return", icon: ic.ret(), separator: true, run: () => leaveAdmin() });
  return items;
}

/// Menu for something right-clicked in the content pane, or null to leave the
/// browser's own menu alone. Only resource surfaces (cards, dashboard rows,
/// tabs) are claimed: text, the XML and log panes and every form field keep
/// the native menu, so copy and paste still work there.
function contentMenuItems(target: Element): CtxItem[] | null {
  if (target.closest("input, textarea, select, .cmd, pre, .select-popover")) return null;
  const el = target.closest<HTMLElement>(
    "[data-vm-open], [data-dc-vm], [data-net-open], [data-dash-net], [data-pool-open], " +
    "[data-dash-pool], [data-open-conn-row], [data-dc-dash-conn], .tab[data-tab], .tab[data-vmtab], " +
    ".tab[data-dctab], .dhead",
  );
  if (!el) return null;
  const d = el.dataset;
  if (d.vmOpen && selectedConn) return vmMenu(selectedConn, d.vmOpen);
  if (d.dcVm && d.dcVmConn) return vmMenu(d.dcVmConn, d.dcVm);
  if (d.netOpen && selectedConn) return netMenu(selectedConn, d.netOpen);
  if (d.dashNet && d.dashConn) return netMenu(d.dashConn, d.dashNet);
  if (d.poolOpen && selectedConn) return poolMenu(selectedConn, d.poolOpen);
  if (d.dashPool && d.dashConn) return poolMenu(d.dashConn, d.dashPool);
  if (d.openConnRow) return serverMenu(d.openConnRow);
  if (d.dcDashConn) return serverMenu(d.dcDashConn);
  // A server tab: the server-level actions.
  if (d.tab && selectedConn && !selectedVm) {
    return serverMenu(selectedConn);
  }
  if (d.vmtab && selectedConn && selectedVm) return vmMenu(selectedConn, selectedVm);
  if (d.dctab) return rootMenu();
  // The page header stands for the page's own resource.
  if (el.matches(".dhead")) {
    if (!selectedConn) return rootMenu();
    if (selectedVm) return vmMenu(selectedConn, selectedVm);
    if (selectedNet) return netMenu(selectedConn, selectedNet);
    if (selectedPool) return poolMenu(selectedConn, selectedPool);
    return serverMenu(selectedConn);
  }
  return null;
}

/// Open the actions for whatever resource is under `target`, at viewport
/// coordinates. Returns whether anything was offered.
///
/// This is the whole of the right-click behaviour, and touch reuses it: a
/// long press on a phone has to open the SAME menu, built by the same `*Menu`
/// functions, or every action would need a second, touch-only definition.
function menuAt(target: Element | null, x: number, y: number): boolean {
  if (!target?.closest) return false;
  let items: CtxItem[] | null = null;
  if (adminSection) {
    // The admin page has its own actions: a section in the sidebar offers
    // open and its common action; a row offers exactly its own buttons.
    if (target.closest(".list-pane")) items = adminTreeMenu(target.closest<HTMLElement>("[data-admin-section]"));
    else if (target.closest("#content") && !target.closest("input, textarea, select, .cmd, pre, .select-popover")) {
      items = adminRowMenu(target);
    }
  } else if (target.closest(".list-pane")) {
    items = treeMenuItems(target.closest(".tree-row") as HTMLElement | null);
  } else if (target.closest("#content")) {
    items = contentMenuItems(target);
  }
  if (!items) return false;
  openContextMenu(x, y, items);
  return true;
}

/// Right-click opens the actions for whatever resource is under the cursor:
/// any row in the sidebar, and the cards, dashboard rows, tabs and page header
/// in the content pane. Delegated from `document` so re-rendering never stacks
/// listeners. Anywhere else the native menu is left alone, so copy/paste still
/// works in the XML and log panes and in every form field.
document.addEventListener("contextmenu", (e) => {
  if (menuAt(e.target as Element | null, e.clientX, e.clientY)) e.preventDefault();
});

// A long press is the phone's right-click. iOS WebKit never fires
// `contextmenu` at all, so without this NOTHING in the sidebar or on a card is
// reachable on a phone — Start, Stop, Delete and Add Resource have no other
// surface. The handler returning false leaves the press to the platform, which
// is what keeps the copy callout in the XML and log panes.
attachLongPress(document, (x, y, target) => menuAt(target, x, y));

/// Refresh the guest list backing the tree for the selected server.
async function refreshTreeVms() {
  // Every server's guests, networks and pools, not just the selected one:
  // the tree auto-expands all three, so an unselected server still has to
  // show its resources.
  const results = await Promise.all(
    connections.map(async (c) => {
      try {
        const [vms, nets, pools] = await Promise.all([
          api.get<DomainSummary[]>(`/proxy/${c.id}/domains`),
          api.get<NetworkSummary[]>(`/proxy/${c.id}/networks`),
          api.get<PoolSummary[]>(`/proxy/${c.id}/pools`),
        ]);
        return [c.id, vms, nets, pools] as const;
      } catch {
        // Unreachable daemon: keep whatever we last knew rather than
        // collapsing its branch. The content pane surfaces the error.
        return [c.id, treeVms[c.id] ?? [], treeNets[c.id] ?? [], treePools[c.id] ?? []] as const;
      }
    }),
  );

  let changed = false;
  for (const [id, vms, nets, pools] of results) {
    if (JSON.stringify(treeVms[id]) !== JSON.stringify(vms)) {
      treeVms[id] = vms;
      changed = true;
    }
    if (JSON.stringify(treeNets[id]) !== JSON.stringify(nets)) {
      treeNets[id] = nets;
      changed = true;
    }
    if (JSON.stringify(treePools[id]) !== JSON.stringify(pools)) {
      treePools[id] = pools;
      changed = true;
    }
  }
  // Only re-render when something actually moved, so the tree does not
  // flicker or fight the user's clicks on every poll.
  if (changed) {
    syncVmActions();
    renderTree();
    // The Datacenter view lists the same guests, so keep it in step; without
    // this its Guests table stays empty until another action redraws it.
    // Not on the admin page: it has no server selected either, and
    // repainting the datacenter over it threw the user off it.
    if (!selectedConn && !selectedVm && !adminSection) renderDc();
    // Network/pool pages poll their own data; the tree refresh above is what
    // keeps the sidebar counts and states live.
  }
}

function renderProfileBtn() {
  const el = $("#profile-btn");
  if (!el || !user) return;
  el.onclick = openProfile;
  setHtml(el, `
    <div class="avatar">${avatarHtml()}</div>
    <div class="who"><b>${esc(user.nickname)}</b><span>${user.admin ? "administrator" : "user"}</span></div>`);
}

// ---------------------------------------------------------------- datacenter

function renderDc() {
  beginRender();
  syncUrl();
  const content = $("#content");
  // Real tabbar; the Users tab is hidden when the local app disables auth.
  // Actions live on the tab bar itself, right-aligned after the tabs.
  const tabs = `<div class="tabbar">
      <button class="tab ${dcTab === "dashboard" ? "active" : ""}" data-dctab="dashboard">${ic.datacenter(14)} Dashboard</button>
      <button class="tab ${dcTab === "guests" ? "active" : ""}" data-dctab="guests">${ic.vms(14)} Guests</button>
    </div>`;
  // Aggregate cards across every known server, from the tree's guest cache.
  const allVms = Object.values(treeVms).flat();
  const runningVms = allVms.filter((v) => v.active).length;
  // Summary cards head both the Servers and Guests tabs.
  const cards = `<div class="cards">
        <div class="card">
          <span class="card-ic">${ic.server(20)}</span>
          <div><b>${connections.length}</b><i>SERVERS</i></div>
        </div>
        <div class="card">
          <span class="card-ic">${ic.vms(20)}</span>
          <div><b>${allVms.length}</b><i>GUESTS</i></div>
        </div>
        <div class="card ${runningVms > 0 ? "ok" : ""}">
          <span class="card-ic">${ic.start(20)}</span>
          <div><b>${runningVms}</b><i>RUNNING</i></div>
        </div>
        <div class="card">
          <span class="card-ic">${ic.stop(20)}</span>
          <div><b>${allVms.length - runningVms}</b><i>STOPPED</i></div>
        </div>
      </div>`;
  // Every guest across every server, so a VM is reachable directly from here
  // instead of only through the sidebar.
  const guestCards = connections
    .flatMap((c) => (treeVms[c.id] ?? []).map((v) => ({ conn: c, vm: v })))
    .map(({ conn, vm }) => `
      <div class="storage-card ${vm.active ? "ok" : ""}" data-open tabindex="0" data-dc-vm="${esc(vm.name)}" data-dc-vm-conn="${conn.id}">
        <div class="storage-card-head">
          <span class="storage-card-icon">${ic.vm()}</span>
          <span class="storage-card-title">${esc(vm.title || vm.name)}</span>
          <span class="badge">${esc(conn.name)}</span>
          <span class="badge ${vm.active ? "on" : ""}">${vm.active ? "Active" : "Inactive"}</span>
        </div>
        <div class="specs">
          <div class="spec"><b>vCPUs</b><span>${vm.vcpus || "—"}</span></div>
          <div class="spec"><b>Memory</b><span>${vm.memory_mb ? fmtBytes(vm.memory_mb * 1024 * 1024) : "—"}</span></div>
        </div>
        <div class="storage-card-actions net-actions">
          <button class="btn icon ${vm.active ? "on" : ""}" data-dc-toggle="${esc(vm.name)}" data-dc-toggle-conn="${conn.id}" data-dc-action="${vm.active ? "stop" : "start"}" title="${vm.active ? "Shutdown" : "Start"}">${vm.active ? ic.stop() : ic.start()}</button>
          <button class="btn icon" data-dc-console="${esc(vm.name)}" data-conn="${conn.id}" title="Console">${ic.vm()}</button>
          <button class="btn icon" data-dc-hwedit="${esc(vm.name)}" data-conn="${conn.id}" title="Edit hardware">${ic.edit()}</button>
        </div>
      </div>`).join("");


  let body: string;
  if (dcTab === "dashboard") {
    body = `<div id="dc-dash">${skeleton("dashboard")}</div>`;
  } else if (dcTab === "guests") {
    body = `${cards}<div class="section-title">Guests</div>
       ${guestCards ? `<div class="card-grid">${guestCards}</div>` : emptyState("guests", "Create Guest", "dc-guest-create")}`;
  } else {
    body = `<div id="users-area">${skeleton("table")}</div>`;
  }
  const restoreTabFocus = holdFocus(content, ".tab.active");
setHtml(content, `<div class="dhead"><span class="dtitle">Datacenter</span></div>${tabs}${body}`);
  restoreTabFocus();

  if (dcTab === "dashboard") void loadDcDashboard();

  $("#dc-guest-create")?.addEventListener("click", () => openCreateVm());
  content.querySelectorAll("[data-dctab]").forEach((b: Element) => {
    (b as HTMLElement).onclick = () => {
      dcTab = (b as HTMLElement).dataset.dctab as typeof dcTab;
      renderDc();
    };
  });
  /// Open a guest from the Datacenter cards, switching server if needed.
  const openVm = (connId: string, vm: string, tab: typeof vmTab) => {
    leaveCurrentView();
    selectedConn = connId;
    selectedVm = vm;
    vmTab = tab;
    renderTree();
    void renderVm();
  };
  content.querySelectorAll("[data-dc-vm]").forEach((card: Element) => {
    (card as HTMLElement).onclick = () =>
      openVm((card as HTMLElement).dataset.dcVmConn!, (card as HTMLElement).dataset.dcVm!, "summary");
  });
  content.querySelectorAll("[data-dc-toggle]").forEach((b: Element) => {
    const el = b as HTMLElement;
    el.onclick = async (e) => {
      e.stopPropagation();
      const start = el.dataset.dcAction === "start";
      await act(`/proxy/${el.dataset.dcToggleConn}/domains/${encodeURIComponent(el.dataset.dcToggle!)}/${start ? "start" : "shutdown"}`, "POST");
      await refreshTreeVms();
      renderDc();
    };
  });
  content.querySelectorAll("[data-dc-console]").forEach((b: Element) => {
    const el = b as HTMLElement;
    el.onclick = (e) => {
      e.stopPropagation();
      openVm(el.dataset.conn!, el.dataset.dcConsole!, "console");
    };
  });
  content.querySelectorAll("[data-dc-hwedit]").forEach((b: Element) => {
    const el = b as HTMLElement;
    el.onclick = (e) => {
      e.stopPropagation();
      // The editor reads through selectedConn, so point at the owning server.
      selectedConn = el.dataset.conn!;
      void openHardwareEditor({ vm: el.dataset.dcHwedit! });
    };
  });

}

/// Datacenter dashboard: aggregate every reachable server's host metrics into
/// one cluster view — total CPU, memory, disk, and network, plus per-server
/// rows that open that server's own dashboard. Same widgets as the daemon
/// dashboard, so the two reads are visually consistent.
async function loadDcDashboard() {
  const body = $("#dc-dash");
  if (!body) return;

  const per = await Promise.all(
    connections.map(async (c) => {
      try {
        const [h, nets, pools] = await Promise.all([
          api.get<HostStats>(`/proxy/${c.id}/host/stats`),
          api.get<NetworkSummary[]>(`/proxy/${c.id}/networks`),
          api.get<PoolSummary[]>(`/proxy/${c.id}/pools`),
        ]);
        return { c, h, nets, pools } as const;
      } catch {
        return null; // unreachable daemon: skip, the Servers tab still lists it
      }
    }),
  );
  const live = per.filter((x) => x !== null);
  const vms = connections.flatMap((c) => treeVms[c.id] ?? []);
  const running = vms.filter((v) => v.active).length;

  // Cluster CPU: sum of busy jiffies over sum of total jiffies — a true
  // cluster-wide utilisation, not an arithmetic mean of per-host percentages
  // (which would weight a 2-core host equally with a 64-core one).
  const agg: HostStats = {
    cpu_total: 0,
    cpu_idle: 0,
    mem_total_kib: 0,
    mem_available_kib: 0,
    cpus: 0,
    sampled_at_ms: 0,
  };
  const nets: { c: Connection; n: NetworkSummary }[] = [];
  const pools: { c: Connection; p: PoolSummary }[] = [];
  for (const { c, h, nets: ns, pools: ps } of live) {
    agg.cpu_total += h.cpu_total;
    agg.cpu_idle += h.cpu_idle;
    agg.mem_total_kib += h.mem_total_kib;
    agg.mem_available_kib += h.mem_available_kib;
    agg.cpus += h.cpus;
    agg.sampled_at_ms = Math.max(agg.sampled_at_ms, h.sampled_at_ms);
    for (const n of ns) nets.push({ c, n });
    for (const p of ps) pools.push({ c, p });
  }

  const live1 = addHostSample("datacenter", agg);
  const memUsedKib = agg.mem_total_kib - agg.mem_available_kib;
  const diskTotal = pools.reduce((a, { p }) => a + (p.capacity ?? 0), 0);
  const diskUsed = pools.reduce((a, { p }) => a + (p.allocation ?? 0), 0);
  const diskLive = diskTotal > 0 ? Math.max(0, Math.min(100, (diskUsed / diskTotal) * 100)) : 0;
  const diskHist1 = addDiskSample("datacenter", diskLive);
  // A stored range replaces the live minute; the headline figures stay live.
  let cpu: (number | null)[] = live1.cpu;
  let mem: (number | null)[] = live1.mem;
  let disk: (number | null)[] = diskHist1;
  let timing: Timing | undefined;
  let diskTiming: Timing | undefined;
  if (!isLive()) {
    const shown = range().key;
    try {
      const [h, d] = await Promise.all([
        loadHistory(api.get, { kind: "dc" }),
        loadHistory(api.get, { kind: "dcdisk" }),
      ]);
      ({ a: cpu, b: mem, timing } = h);
      ({ a: disk, timing: diskTiming } = d);
    } catch { /* keep the live minute */ }
    // The range or the view changed while the history was in flight.
    if (range().key !== shown || !$("#dc-dash")) return;
  }
  const { cpuNow, memNow } = live1;
  const diskNow = diskLive;
  const activeNets = nets.filter(({ n }) => n.active).length;
  const activePools = pools.filter(({ p }) => p.active).length;
  const byId = new Map(live.map(({ c, h, pools: ps }) => [c.id, { h, pools: ps }] as const));

  setHtml(body, `
    ${rangeBar()}
    <div class="cards">
      <div class="card"><span class="card-ic">${ic.server(20)}</span><div><b>${connections.length}</b><i>SERVERS</i></div></div>
      <div class="card"><span class="card-ic">${ic.vms(20)}</span><div><b>${vms.length}</b><i>GUESTS</i></div></div>
      <div class="card ${running > 0 ? "ok" : ""}"><span class="card-ic">${ic.start(20)}</span><div><b>${running}</b><i>RUNNING</i></div></div>
      <div class="card ${activeNets > 0 ? "ok" : ""}"><span class="card-ic">${ic.network(20)}</span><div><b>${nets.length}</b><i>NETWORKS</i></div></div>
      <div class="card ${activePools > 0 ? "ok" : ""}"><span class="card-ic">${ic.storage(20)}</span><div><b>${pools.length}</b><i>POOLS</i></div></div>
    </div>

    <div class="sumgrid">
      <div class="metric ok">
        <div class="metric-head">
          <span class="metric-ic">${ic.cpu()}</span>
          <b>Total CPU</b>
          <span class="metric-value">${pct(cpuNow)}</span>
        </div>
        <div class="meter"><div class="meter-fill ${cpuNow > 85 ? "hot" : ""}" style="width:${cpuNow.toFixed(1)}%"></div></div>
        <div class="chart">
          ${sparkline(cpu, "", 100, timing)}
          <div class="chart-axis"><span>${range().axis}</span><span>now</span></div>
        </div>
        <div class="metric-foot"><span>${agg.cpus} cores</span><span>${live.length} servers</span></div>
      </div>

      <div class="metric ok">
        <div class="metric-head">
          <span class="metric-ic">${ic.memory()}</span>
          <b>Total Memory</b>
          <span class="metric-value">${pct(memNow)}</span>
        </div>
        <div class="meter"><div class="meter-fill ${memNow > 85 ? "hot" : ""}" style="width:${memNow.toFixed(1)}%"></div></div>
        <div class="chart">
          ${sparkline(mem, "mem", 100, timing)}
          <div class="chart-axis"><span>${range().axis}</span><span>now</span></div>
        </div>
        <div class="metric-foot">
          <span>${fmtBytes(memUsedKib * 1024)} used</span>
          <span>${fmtBytes(agg.mem_total_kib * 1024)} total</span>
        </div>
      </div>

      <div class="metric ok">
        <div class="metric-head">
          <span class="metric-ic">${ic.disk()}</span>
          <b>Total Disk</b>
          <span class="metric-value">${diskNow.toFixed(diskNow >= 10 ? 0 : 1)}%</span>
        </div>
        <div class="meter"><div class="meter-fill ${diskNow > 85 ? "hot" : ""}" style="width:${diskNow.toFixed(1)}%"></div></div>
        <div class="chart">
          ${sparkline(disk, "", 100, diskTiming)}
          <div class="chart-axis"><span>${range().axis}</span><span>now</span></div>
        </div>
        <div class="metric-foot">
          <span>${fmtBytes(diskUsed)} used</span>
          <span>${fmtBytes(diskTotal)} total</span>
        </div>
      </div>
    </div>

    <div class="dash-section">
      <div class="section-title">Servers</div>
      <table class="grid">
        <tr><th>Server</th><th>CPU</th><th>Memory</th><th>Disk</th></tr>
        ${connections.map((c) => {
          const row = byId.get(c.id);
          if (!row) {
            return `<tr><td class="tname" style="cursor:pointer" tabindex="0" data-dc-dash-conn="${c.id}">${esc(c.name)} <span class="state bad">Offline</span></td><td colspan="3" class="muted">${esc(c.offline_error ?? "not answering")}</td></tr>`;
          }
          const { h, pools: ps } = row;
          const { cpuNow: sCpu } = addHostSample(`dc:${c.id}`, h);
          const mUsed = h.mem_total_kib - h.mem_available_kib;
          const mPct = h.mem_total_kib ? Math.max(0, Math.min(100, (mUsed / h.mem_total_kib) * 100)) : 0;
          const dTotal = ps.reduce((a, p) => a + (p.capacity ?? 0), 0);
          const dUsed = ps.reduce((a, p) => a + (p.allocation ?? 0), 0);
          const dPct = dTotal > 0 ? Math.max(0, Math.min(100, (dUsed / dTotal) * 100)) : 0;
          return `
          <tr>
            <td class="tname" style="cursor:pointer" tabindex="0" data-dc-dash-conn="${c.id}">${esc(c.name)}</td>
            <td>${pct(sCpu)}</td>
            <td>${mPct.toFixed(0)}% · ${fmtBytes(mUsed * 1024)}</td>
            <td>${dPct.toFixed(0)}% · ${fmtBytes(dUsed)}</td>
          </tr>`;
        }).join("")}
      </table>
    </div>

    <div class="dash-section">
      <div class="section-title">Networking</div>
      <table class="grid">
        <tr><th>Name</th><th>Server</th><th>Bridge</th><th>State</th></tr>
        ${nets.map(({ c, n }) => `
          <tr class="row-open" tabindex="0" data-dash-net="${esc(n.name)}" data-dash-conn="${c.id}"><td>${esc(n.name)}</td><td>${esc(c.name)}</td><td>${esc(n.bridge)}</td>
          <td><span class="state ${n.active ? "on" : ""}">${n.active ? "Active" : "Inactive"}</span></td></tr>`).join("") || `<tr><td colspan="4" class="empty">No networks.</td></tr>`}
      </table>
    </div>

    <div class="dash-section">
      <div class="section-title">Storage Pools</div>
      ${pools.map(({ c, p }) => {
        const used = p.allocation ?? 0;
        const cap = p.capacity ?? 0;
        const usedPct = cap > 0 ? Math.max(0, Math.min(100, (used / cap) * 100)) : 0;
        return `
        <div class="storage-card ${p.active ? "ok" : ""}" data-open tabindex="0" data-dash-pool="${esc(p.name)}" data-dash-conn="${c.id}">
          <div class="storage-card-head">
            <span class="storage-card-icon">${ic.storage()}</span>
            <span class="storage-card-title">${esc(p.name)}<span class="host" style="display:block;font-weight:400">${esc(c.name)}</span></span>
            <span class="badge ${p.active ? "on" : ""}">${p.active ? "Active" : "Inactive"}</span>
          </div>
          <div class="metric-value">${usedPct.toFixed(usedPct >= 10 ? 0 : 1)}%</div>
          <div class="meter"><div class="meter-fill ${usedPct > 85 ? "hot" : ""}" style="width:${usedPct.toFixed(1)}%"></div></div>
          <div class="metric-foot">
            <span>${fmtBytes(used)} used</span>
            <span>${fmtBytes(cap)} total</span>
          </div>
        </div>`;
      }).join("") || `<div class="empty">No pools.</div>`}
    </div>
    `);

  bindDashOpeners(body);
  body.querySelectorAll("[data-dc-dash-conn]").forEach((r: Element) => {
    (r as HTMLElement).onclick = () => {
      selectedConn = (r as HTMLElement).dataset.dcDashConn!;
      selectedVm = null;
      connTab = "dashboard";
      renderTree();
      void renderConn();
    };
  });
}

// ----------------------------------------------------------- admin page

/// Open the admin page at `section` (or where it was last).
function openAdmin(section: AdminSection = adminSection ?? "dashboard", tab?: AdminTab) {
  if (!canAdmin()) return;
  if (!adminSection) lastNonAdminRoute = currentRoute();
  leaveCurrentView();
  clearResourceSelection();
  selectedConn = null;
  // Staying in a section keeps its tab; entering one starts at its first.
  if (tab) adminTab = tab;
  else if (section !== adminSection || !adminTab) adminTab = ADMIN_TABS[section]?.[0];
  adminSection = section;
  renderTree();
  renderAdmin();
}

/// The return button: back to the view the admin page was opened from, or
/// the datacenter when /admin was the first page loaded.
function leaveAdmin() {
  applyRoute(lastNonAdminRoute ?? { view: "dc", tab: "dashboard" });
  adminSection = null;
  renderCurrent();
}

/// The admin button stays where it is and becomes a return button while the
/// admin page is open.
function syncAdminButton() {
  const b = $("#admin-btn");
  if (!b) return;
  b.title = adminSection ? "Return" : "Administration";
  b.setAttribute("aria-label", b.title);
  b.classList.toggle("on", !!adminSection);
  setHtml(b, adminSection ? ic.ret() : ic.admin());
}

const adminCtx: AdminCtx = {
  setHtml,
  $,
  openModal,
  askConfirm,
  toast,
  showSecret,
  relTime,
  me: () => user,
  connections: () => connections,
  reloadConnections: () => loadConnections(),
  openConnectionDialog: (id) => openConnectionDialog(id),
  deleteConnection: (id) => deleteConnection(id),
  openResource: (kind, conn, name) => {
    adminSection = null;
    if (kind === "vm") selectVmRow(conn, name);
    else if (kind === "network") selectNetRow(conn, name);
    else selectPoolRow(conn, name);
  },
  go: (section, tab) => openAdmin(section, tab),
  createVm: (conn, owner, after) => void openHardwareEditor({ conn, forUser: { owner, after } }),
  createNetwork: (conn, owner, after) => openCreateNetwork(conn, { owner, after }),
  tab: () => adminTab,
  refreshMe: async () => {
    user = await api.get<User>("/auth/me");
    await loadConnections();
    if (!canAdmin()) adminSection = null;
    renderApp();
  },
  stillCurrent: (gen) => stillCurrent(gen),
};

function renderAdmin() {
  if (!adminSection) return;
  const gen = beginRender();
  syncUrl();
  const content = $("#content");
  setHtml(content, `<div class="dhead"><span class="dhead-ic">${ic.admin(20)}</span>
      <span class="dtitle">${esc(adminTitle(adminSection))}</span></div>
    <div id="admin-area"></div>`);
  void renderAdminSection(adminCtx, $("#admin-area", content), adminSection, gen);
}


/// Present a secret that the server will never show again.
function showSecret(title: string, secret: string) {
  const el = openModal(
    title,
    `<div class="hw-note">Copy this now — it is not stored and cannot be shown again.</div>
     <code class="cmd secret-value" id="sv">${esc(secret)}</code>`,
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

// ---------------------------------------------------------------- resource pages

/// Return to the selected server's dashboard from a network/pool page.
function backToConn() {
  selectedNet = null;
  selectedPool = null;
  connTab = "dashboard";
  renderTree();
  renderConn();
}

/// A network or pool page whose resource is gone (a stale link, or deleted
/// elsewhere) falls back to the server's list of that kind, replacing the dead
/// URL rather than stacking it in history, instead of a raw libvirt error.
function leaveMissing(tab: "networks" | "storage", message: string) {
  clearResourceSelection();
  connTab = tab;
  restoringRoute = true;
  try {
    renderTree();
    renderConn();
  } finally {
    restoringRoute = false;
  }
  toast(message, "bad");
}

/// Dedicated network page: card-based information, an IPAM allocation list
/// (MAC -> IP DHCP hosts) and the lifecycle/edit actions.
/// A network's DHCP range, DNS forwarders and domain. Stored by this host's
/// IPAM, so saving never touches the libvirt network or its guests.
function openIpamSettings(conn: string, net: string, ipam: IpamInfo) {
  const el = openModal(`DHCP & DNS — ${esc(net)}`, `
    <div class="grid2">
      <label class="field"><span>Range start</span><input id="ipam-rs" value="${esc(ipam.range_start)}"></label>
      <label class="field"><span>Range end</span><input id="ipam-re" value="${esc(ipam.range_end)}"></label>
    </div>
    <label class="field"><span>DNS forwarders (blank = the host's own resolvers)</span>
      <input id="ipam-fwd" value="${esc(ipam.dns_forwarders.join(", "))}" placeholder="1.1.1.1, 9.9.9.9"></label>
    <label class="field"><span>Domain (optional)</span>
      <input id="ipam-dom" value="${esc(ipam.domain ?? "")}" placeholder="lab"></label>
    <div class="dsub">Guests get addresses from the range, and keep them until they are deleted;
      changing the range never moves one. Guest names resolve as <code>name</code>${ipam.domain ? ` and <code>name.${esc(ipam.domain)}</code>` : ""}.</div>`,
    `<button class="btn primary" id="ipam-set-ok">Save</button>`);
  $("#ipam-set-ok", el).onclick = async () => {
    try {
      await api.put(`/proxy/${conn}/networks/${encodeURIComponent(net)}/ipam`, {
        range_start: ($("#ipam-rs", el) as HTMLInputElement).value.trim(),
        range_end: ($("#ipam-re", el) as HTMLInputElement).value.trim(),
        dns_forwarders: ($("#ipam-fwd", el) as HTMLInputElement).value.split(/[\s,]+/).filter(Boolean),
        domain: ($("#ipam-dom", el) as HTMLInputElement).value.trim() || null,
      });
      el.remove();
      toast("DHCP & DNS settings saved");
      if (selectedNet === net) void renderNetPage();
    } catch (e) { toast((e as Error).message, "bad"); }
  };
}

async function renderNetPage() {
  if (!selectedConn || !selectedNet) return;
  if (isOffline(selectedConn)) return renderOffline();
  const gen = beginRender();
  syncUrl();
  const conn = selectedConn; // narrowed copy for nested closures
  const content = $("#content");
  const name = selectedNet;
  try {
    const netBase = `/proxy/${selectedConn}/networks/${encodeURIComponent(name)}`;
    const [summary, xml, ipamState] = await Promise.all([
      api.get<NetworkSummary[]>(`/proxy/${selectedConn}/networks`),
      api.get<{ xml: string }>(`${netBase}/xml`),
      // Absent for a network this host does not serve (bridge mode, or no
      // IPv4 subnet): no IPAM section then.
      Promise.all([
        api.get<IpamInfo>(`${netBase}/ipam`),
        api.get<{ hosts: IpamHost[] }>(`${netBase}/hosts`),
      ]).catch(() => null),
    ]);
    // The poll may have started this render and the user then switched panes;
    // painting now would yank their view back to the network page.
    if (!stillCurrent(gen)) return;
    const net = summary.find((n) => n.name === name);
    if (!net) return leaveMissing("networks", `Network "${name}" no longer exists`);
    const form = networkFormFromXml(xml.xml);
    const ipam = ipamState?.[0] ?? null;
    const typeLabel = form.forward === "bridge" ? "Bridge" : form.forward === "nat" ? "NAT" : "Isolated";
    const gateway = form.ip ? form.ip.split("/")[0] : "—";
    const hosts = ipamState?.[1].hosts ?? [];
    const dhcpRange = ipam ? { start: ipam.range_start, end: ipam.range_end } : null;
    const remaining = ipam ? { free: ipam.range_size - ipam.range_used, total: ipam.range_size } : null;
    // Guests ask the gateway, which answers their names and forwards the rest.
    const dnsLabel = ipam
      ? `${ipam.gateway}${ipam.dns_forwarders.length ? ` → ${ipam.dns_forwarders.join(", ")}` : ""}`
      : "—";
    const hostsUrl = `${netBase}/hosts`;

    setHtml(content, `
      <div class="dhead">
        <span class="dhead-ic">${ic.network(20)}</span>
        <span class="dtitle">${esc(name)}</span>
        <span class="badge ${net.active ? "on" : ""}">${net.active ? "Active" : "Inactive"}</span>
      </div>
      <div class="actions" style="margin-top:12px">
        <button class="btn ${net.active ? "on" : ""}" id="net-toggle">${net.active ? ic.stop(14) : ic.start(14)} ${net.active ? "Stop" : "Start"}</button>
        <button class="btn ${net.autostart ? "on" : ""}" id="net-autostart">${ic.autostart(14)} Autostart ${net.autostart ? "on" : "off"}</button>
        <button class="btn primary" id="net-edit">${ic.edit(14)} Edit</button>
        <button class="btn" id="net-fw">${ic.firewall(14)} Firewall</button>
        <button class="btn danger" id="net-del">${ic.trash(14)} Delete</button>
      </div>
      <div class="cards">
        <div class="card"><span class="card-ic">${ic.network(20)}</span><div><b>${esc(typeLabel)}</b><i>TYPE</i></div></div>
        <div class="card"><span class="card-ic">${ic.server(20)}</span><div><b>${esc(form.bridge || "—")}</b><i>BRIDGE</i></div></div>
        <div class="card"><span class="card-ic">${ic.circleDot(20)}</span><div><b>${esc(form.ip || "—")}</b><i>SUBNET</i></div></div>
        <div class="card"><span class="card-ic">${ic.chevron(20)}</span><div><b>${esc(gateway)}</b><i>GATEWAY</i></div></div>
        <div class="card"><span class="card-ic">${ic.circle(20)}</span><div><b>${remaining ? `${remaining.free} / ${remaining.total}` : "—"}</b><i>DHCP IPS REMAINING</i></div></div>
        <div class="card"><span class="card-ic">${ic.circleDot(20)}</span><div><b>${dhcpRange ? `${esc(dhcpRange.start)} – ${esc(dhcpRange.end.split(".").pop() ?? "")}` : "—"}</b><i>DHCP RANGE</i></div></div>
        <div class="card"><span class="card-ic">${ic.options(20)}</span><div><b>${esc(dnsLabel)}</b><i>DNS</i></div></div>
        <div class="card"><span class="card-ic">${ic.autostart(20)}</span><div><b>${net.persistent ? "Persistent" : "Transient"}</b><i>PERSISTENCE</i></div></div>
      </div>

      ${ipam ? `
      <div class="section-title fw-cat">IPAM Allocations</div>
      <div class="dsub" style="margin-bottom:8px">${ipam.serving
        ? "Served by this host. A guest with no allocation gets one from the range and keeps it until the VM is deleted."
        : "Not being served right now (the network is stopped)."}</div>
      <table class="grid" id="ipam-hosts">
        <tr><th>Name</th><th>VM</th><th>MAC address</th><th>IP address</th><th>Last seen</th><th></th></tr>
        ${hosts.map((h, i) => `
          <tr data-ipam-mac="${esc(h.mac)}">
            <td>${esc(h.name || "—")}</td>
            <td>${esc(h.vm_name ?? "—")}</td>
            <td>${esc(h.mac)}</td>
            <td>${esc(h.ip)}${h.dynamic ? ` <span class="state" title="Assigned from the range when the guest first asked">auto</span>` : ""}</td>
            <td>${h.seen_ms ? `${ago(h.seen_ms)} ago` : "—"}</td>
            <td class="col-actions"><div class="row-actions">
              <button class="btn icon" data-ipam-edit="${i}" title="Edit">${ic.edit()}</button>
              <button class="btn danger icon" data-ipam-del="${i}" title="Release">${ic.trash()}</button>
            </div></td>
          </tr>`).join("") || `<tr><td colspan="6" class="empty">No allocations yet — guests get one from the range when they first ask.</td></tr>`}
      </table>
      <div class="actions">
        <button class="btn" id="ipam-add">${ic.plus(14)} Add allocation</button>
        <button class="btn" id="ipam-settings">${ic.options(14)} DHCP &amp; DNS settings</button>
      </div>` : ""}`);

    const editHost = (idx: number | null) => {
      const h = idx === null ? { mac: "", ip: "", name: "" } : hosts[idx];
      const vms = treeVms[conn] ?? [];
      const el = openModal(
        idx === null ? "Add IPAM allocation" : "Edit IPAM allocation",
        `<div class="grid2">
          <label class="field"><span>VM (optional)</span>
            <select id="ipam-vm">
              <option value="">Manual entry</option>
              ${vms.map((v) => `<option value="${esc(v.name)}" ${v.name === h.name ? "selected" : ""}>${esc(v.title || v.name)}</option>`).join("")}
            </select></label>
          <label class="field" id="ipam-nic-wrap" hidden><span>Interface</span><select id="ipam-nic"><option value="">Select an interface…</option></select></label>
          <label class="field"><span>Name (optional)</span><input id="ipam-name" value="${esc(h.name ?? "")}" placeholder="guest hostname"></label>
          <label class="field"><span>IP address</span><input id="ipam-ip" value="${esc(h.ip)}" placeholder="192.168.100.50"></label>
          <label class="field"><span>MAC address</span><input id="ipam-mac" value="${esc(h.mac)}" placeholder="52:54:00:aa:bb:cc"></label>
        </div>`,
        `<button class="btn primary" id="ipam-save">${idx === null ? "Add" : "Save"}</button>`,
      );

      const vmSel = $("#ipam-vm", el) as HTMLSelectElement;
      const nicSel = $("#ipam-nic", el) as HTMLSelectElement;
      const nicWrap = $("#ipam-nic-wrap", el) as HTMLElement;
      const nameInp = $("#ipam-name", el) as HTMLInputElement;
      const ipInp = $("#ipam-ip", el) as HTMLInputElement;
      const macInp = $("#ipam-mac", el) as HTMLInputElement;

      // Picking a VM offers its NIC MACs; picking one fills MAC, name and a
      // free IP from the DHCP range. Manual entry still works by hand-typing
      // the MAC.
      const pickVm = async (vmName: string) => {
        if (!vmName) {
          nicWrap.hidden = true;
          return;
        }
        try {
          const res = await api.get<{ interfaces: { mac: string | null }[] }>(
            `/proxy/${selectedConn}/domains/${encodeURIComponent(vmName)}/interfaces`);
          const macs = (res.interfaces ?? []).map((x) => x.mac).filter(Boolean) as string[];
          setHtml(nicSel, macs.map((m) => `<option value="${esc(m)}">${esc(m)}</option>`).join("") || `<option value="">No NIC</option>`);
          nicWrap.hidden = false;
          if (macs.length === 1) {
            nicSel.value = macs[0];
            macInp.value = macs[0];
            if (!nameInp.value) nameInp.value = vmName;
            if (!ipInp.value) {
              ipInp.value = nextFreeIp(form.ip, hosts.map((x) => x.ip), dhcpRange ?? undefined) ?? "";
            }
          }
        } catch {
          nicWrap.hidden = true;
        }
      };
      vmSel.addEventListener("change", () => void pickVm(vmSel.value));
      nicSel.addEventListener("change", () => {
        macInp.value = nicSel.value;
        if (!ipInp.value) {
          ipInp.value = nextFreeIp(form.ip, hosts.map((x) => x.ip), dhcpRange ?? undefined) ?? "";
        }
      });

      $("#ipam-save", el).onclick = async () => {
        const mac = macInp.value.trim();
        const ip = ipInp.value.trim();
        if (!mac || !ip) return toast("MAC and IP are required", "bad");
        try {
          // Editing an allocation to a different MAC is a move: drop the old.
          const old = idx === null ? null : hosts[idx];
          if (old && old.mac !== mac.toLowerCase()) await api.del(`${hostsUrl}/${encodeURIComponent(old.mac)}`);
          await api.put(hostsUrl, { mac, ip, name: nameInp.value.trim() || undefined });
          el.remove();
          toast("Allocation saved — the guest gets it at its next DHCP exchange");
          void renderNetPage();
        } catch (e) { toast((e as Error).message, "bad"); }
      };
    };

    $("#net-toggle", content).onclick = async () => {
      await netAction(net.active ? "stop" : "start", name);
      void renderNetPage();
    };
    $("#net-autostart", content).onclick = async () => {
      await toggleAutostart(name);
      void renderNetPage();
    };
    $("#net-edit", content).onclick = () => openEditNetwork(name);
    $("#net-fw", content).onclick = () => openFirewallEditor(name);
    $("#net-del", content).onclick = async () => {
      await deleteNetwork(name);
      backToConn();
    };
    const addBtn = $("#ipam-add", content);
    if (addBtn) addBtn.onclick = () => editHost(null);
    const setBtn = $("#ipam-settings", content);
    if (setBtn && ipam) setBtn.onclick = () => openIpamSettings(conn, name, ipam);
    content.querySelectorAll("[data-ipam-edit]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => editHost(+(b as HTMLElement).dataset.ipamEdit!);
    });
    content.querySelectorAll("[data-ipam-del]").forEach((b: Element) => {
      (b as HTMLElement).onclick = async () => {
        const h = hosts[+(b as HTMLElement).dataset.ipamDel!];
        if (!h) return;
        try {
          await api.del(`${hostsUrl}/${encodeURIComponent(h.mac)}`);
          toast(h.vm_name
            ? `Released — ${h.vm_name} gets a new address from the range at its next DHCP`
            : "Allocation released");
          void renderNetPage();
        } catch (e) { toast((e as Error).message, "bad"); }
      };
    });
  } catch (e) {
    if (!stillCurrent(gen)) return;
    // The XML read fails first when the network is gone (a bookmark, or it was
    // deleted elsewhere), and the daemon reports that as a 500: ask the list.
    const nets = await api.get<NetworkSummary[]>(`/proxy/${conn}/networks`).catch(() => null);
    if (!stillCurrent(gen)) return;
    if (nets && !nets.some((n) => n.name === name)) {
      return leaveMissing("networks", `Network "${name}" no longer exists`);
    }
    setHtml(content, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

/// Dedicated storage page: one pool's capacity plus full volume management.
async function renderPoolPage() {
  if (!selectedConn || !selectedPool) return;
  if (isOffline(selectedConn)) return renderOffline();
  const gen = beginRender();
  syncUrl();
  const content = $("#content");
  const name = selectedPool;
  const conn = selectedConn;
  try {
    // The single-pool read refreshes the pool before answering, so these
    // figures follow the filesystem; the all-pools list is served from
    // libvirt's cache and would show what was true when the page opened.
    const [p, vols] = await Promise.all([
      api.get<PoolSummary>(`/proxy/${selectedConn}/pools/${encodeURIComponent(name)}`),
      api.get<VolumeSummary[]>(`/proxy/${selectedConn}/pools/${encodeURIComponent(name)}/volumes`),
    ]);
    // Same guard as the network page: never paint over a newer view.
    if (!stillCurrent(gen)) return;
    if (!p) return leaveMissing("storage", `Pool "${name}" no longer exists`);
    const used = p.allocation ?? 0;
    const cap = p.capacity ?? 0;
    const usedPct = cap > 0 ? Math.max(0, Math.min(100, (used / cap) * 100)) : 0;

    setHtml(content, `
      <div class="dhead">
        <span class="dhead-ic">${ic.storage(20)}</span>
        <span class="dtitle">${esc(name)}</span>
        <span class="badge ${p.active ? "on" : ""}">${p.active ? "Active" : "Inactive"}</span>
      </div>
      <div class="dsub">Storage pool</div>
      <div class="actions">
        <button class="btn" id="pool-upload">${ic.upload(14)} Upload ISO</button>
        <button class="btn" id="pool-fetch">${ic.download(14)} From URL</button>
        <button class="btn" id="pool-newvol">${ic.plus(14)} New volume</button>
        <button class="btn danger" id="pool-del">${ic.trash(14)} Delete pool</button>
      </div>
      <div class="metric">
        <div class="metric-head">
          <span class="metric-ic">${ic.storage()}</span>
          <b>Allocation</b>
          <span class="metric-value">${usedPct.toFixed(usedPct >= 10 ? 0 : 1)}%</span>
        </div>
        <div class="meter"><div class="meter-fill ${usedPct > 85 ? "hot" : ""}" style="width:${usedPct.toFixed(1)}%"></div></div>
        <div class="metric-foot">
          <span>${fmtBytes(used)} used</span>
          <span>${fmtBytes(p.available)} free</span>
          <span>${fmtBytes(cap)} total</span>
        </div>
      </div>
      <div id="pool-uploads">${uploadsSection(name)}</div>
      <div id="pool-fetches">${fetchesSection(conn, name)}</div>
      <div class="section-title fw-cat" style="margin-top:18px">Volumes</div>
      <table class="grid">
        <tr><th>Volume</th><th>Size</th><th>Allocated</th><th></th></tr>
        ${vols.map((v) => `
          <tr><td>${esc(v.name)}</td><td>${fmtBytes(v.capacity)}</td><td>${fmtBytes(v.allocation)}</td>
          <td><button class="btn danger icon" data-pvol-del="${esc(v.name)}">${ic.trash()}</button></td></tr>`).join("") || `<tr><td colspan="4" class="empty">No volumes.</td></tr>`}
      </table>`);

    $("#pool-upload", content).onclick = () => openUpload(name);
    $("#pool-fetch", content).onclick = () => openFetch(name);
    $("#pool-newvol", content).onclick = () => openCreateVolume(name);
    $("#pool-del", content).onclick = async () => {
      await deletePool(name);
      backToConn();
    };
    content.querySelectorAll("[data-pvol-del]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => {
        void deleteVolume(name, (b as HTMLElement).dataset.pvolDel!).then(() => renderPoolPage());
      };
    });
    bindUploads();
    bindFetches(conn);
    void pollFetches(conn);
    // Refresh the transfers section on entry: the cached list may predate this
    // visit, and a transfer that finished meanwhile should not look stuck.
    void pollUploads();
  } catch (e) {
    if (!stillCurrent(gen)) return;
    // Same as the network page: a missing pool reads as a 500 from the daemon.
    const pools = await api.get<PoolSummary[]>(`/proxy/${conn}/pools`).catch(() => null);
    if (!stillCurrent(gen)) return;
    if (pools && !pools.some((x) => x.name === name)) {
      return leaveMissing("storage", `Pool "${name}" no longer exists`);
    }
    setHtml(content, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

// ---------------------------------------------------------------- connection view

async function renderConn() {
  if (!selectedConn) return;
  if (isOffline(selectedConn)) return renderOffline();
  const conn = connections.find((c) => c.id === selectedConn);
  if (!conn) return;
  // Server view owns the pane: invalidates any in-flight resource render.
  beginRender();
  syncUrl();
  const content = $("#content");
  const restoreTabFocus = holdFocus(content, ".tab.active");
setHtml(content, `
    <div class="dhead"><span class="dtitle">${esc(conn.name)}</span>
      <span class="badge">daemon</span>
    </div>
    <div class="dsub">${esc(conn.url)}${conn.fingerprint ? " · " + conn.fingerprint : ""}</div>
    <div class="tabbar">
      <button class="tab ${connTab === "dashboard" ? "active" : ""}" data-tab="dashboard">${ic.datacenter(14)} Dashboard</button>
      <button class="tab ${connTab === "vms" ? "active" : ""}" data-tab="vms">${ic.vms(14)} VMs</button>
      <button class="tab ${connTab === "networks" ? "active" : ""}" data-tab="networks">${ic.network(14)} Networks</button>
      <button class="tab ${connTab === "storage" ? "active" : ""}" data-tab="storage">${ic.storage(14)} Storage</button>
      <button class="tab ${connTab === "firewall" ? "active" : ""}" data-tab="firewall">${ic.firewall(14)} Firewall</button>
      <div class="tabbar-actions">
        ${connTab === "dashboard" ? `<button class="btn primary" id="create-resource">${ic.plus(14)} Create Resource</button>` : ""}
        ${connTab === "vms" ? `<button class="btn primary" id="create-vm">${ic.plus(14)} Create VM</button>` : ""}
        ${connTab === "networks" ? `<button class="btn primary" id="create-net">${ic.plus(14)} Create Network</button>` : ""}
        ${connTab === "storage" ? `<button class="btn primary" id="create-pool">${ic.plus(14)} Create Pool</button>` : ""}
      </div>
    </div>
    <div id="conn-body"></div>`);
  restoreTabFocus();
  content.querySelectorAll("[data-tab]").forEach((b: Element) => {
    (b as HTMLElement).onclick = () => {
      connTab = (b as HTMLElement).dataset.tab as typeof connTab;
      // Clear the whole resource selection, not just selectedVm: leaving
      // selectedNet/selectedPool set made the 1s poll re-render that network
      // or pool page about a second after the user switched to a server tab.
      clearResourceSelection();
      renderConn();
    };
  });
  renderConnBody();
}

async function renderConnBody() {
  if (!selectedConn) return;
  const body = $("#conn-body");
  // The create buttons live on the tab bar (rendered by renderConn), so the
  // body only carries the list itself.
  if (connTab === "dashboard") {
    setHtml(body, `<div id="dash">${skeleton("dashboard")}</div>`);
    $("#create-resource")?.addEventListener("click", () => openCreateResource());
    await loadDashboard();
  } else if (connTab === "vms") {
    setHtml(body, `<div id="vms-list" class="card-grid">${skeleton("cards")}</div>`);
    $("#create-vm")?.addEventListener("click", () => openCreateVm());
    await fastLoad(loadVms);
  } else if (connTab === "networks") {
    setHtml(body, `<div id="nets-list" class="card-grid">${skeleton("cards")}</div>`);
    $("#create-net")?.addEventListener("click", () => openCreateNetwork());
    await fastLoad(loadNetworks);
  } else if (connTab === "storage") {
    setHtml(body, `<div id="pools-list">${skeleton("cards")}</div>`);
    renderedPools = null; // fresh container: force a full render on first load
    $("#create-pool")?.addEventListener("click", () => openCreatePool());
    await loadPools();
  } else {
    setHtml(body, `<div id="filters-list">${skeleton("table")}</div>`);
    await fastLoad(loadFilters);
  }
}

/// Daemon dashboard: host CPU/RAM with sparklines, then counts and summaries
/// for VMs, networks, and storage pools. Reuses the existing list endpoints.
async function loadDashboard() {
  if (!selectedConn) return;
  const body = $("#dash");
  if (!body) return;
  try {
    const [h, nets, pools] = await Promise.all([
      api.get<HostStats>(`/proxy/${selectedConn}/host/stats`),
      api.get<NetworkSummary[]>(`/proxy/${selectedConn}/networks`),
      api.get<PoolSummary[]>(`/proxy/${selectedConn}/pools`),
    ]);
    const vms = treeVms[selectedConn] ?? [];
    const running = vms.filter((v) => v.active).length;
    const conn = selectedConn;
    const live1 = addHostSample(`host:${conn}`, h);
    const { cpuNow, memNow } = live1;
    let cpu: (number | null)[] = live1.cpu;
    let mem: (number | null)[] = live1.mem;
    let timing: Timing | undefined;
    if (!isLive()) {
      const shown = range().key;
      try {
        ({ a: cpu, b: mem, timing } = await loadHistory(api.get, { kind: "host", conn }));
      } catch { /* keep the live minute */ }
      if (range().key !== shown || selectedConn !== conn || !$("#dash")) return;
    }
    const memUsedKib = h.mem_total_kib - h.mem_available_kib;
    const activeNets = nets.filter((n) => n.active).length;
    const activePools = pools.filter((p) => p.active).length;

    setHtml(body, `
    ${rangeBar()}
    <div class="cards">
      <div class="card"><span class="card-ic">${ic.vms(20)}</span><div><b>${vms.length}</b><i>VMS</i></div></div>
      <div class="card ${running > 0 ? "ok" : ""}"><span class="card-ic">${ic.start(20)}</span><div><b>${running}</b><i>RUNNING</i></div></div>
      <div class="card ${activeNets > 0 ? "ok" : ""}"><span class="card-ic">${ic.network(20)}</span><div><b>${nets.length}</b><i>NETWORKS</i></div></div>
      <div class="card ${activePools > 0 ? "ok" : ""}"><span class="card-ic">${ic.storage(20)}</span><div><b>${pools.length}</b><i>POOLS</i></div></div>
    </div>

    <div class="sumgrid">
      <div class="metric ok">
        <div class="metric-head">
          <span class="metric-ic">${ic.cpu()}</span>
          <b>CPU</b>
          <span class="metric-value">${pct(cpuNow)}</span>
        </div>
        <div class="meter"><div class="meter-fill ${cpuNow > 85 ? "hot" : ""}" style="width:${cpuNow.toFixed(1)}%"></div></div>
        <div class="chart">
          ${sparkline(cpu, "", 100, timing)}
          <div class="chart-axis"><span>${range().axis}</span><span>now</span></div>
        </div>
        <div class="metric-foot"><span>${h.cpus} CPU</span><span>host load</span></div>
      </div>

      <div class="metric ok">
        <div class="metric-head">
          <span class="metric-ic">${ic.memory()}</span>
          <b>Memory</b>
          <span class="metric-value">${pct(memNow)}</span>
        </div>
        <div class="meter"><div class="meter-fill ${memNow > 85 ? "hot" : ""}" style="width:${memNow.toFixed(1)}%"></div></div>
        <div class="chart">
          ${sparkline(mem, "mem", 100, timing)}
          <div class="chart-axis"><span>${range().axis}</span><span>now</span></div>
        </div>
        <div class="metric-foot">
          <span>${fmtBytes(memUsedKib * 1024)} used</span>
          <span>${fmtBytes(h.mem_total_kib * 1024)} total</span>
        </div>
      </div>
    </div>

    <div class="dash-section">
      <div class="section-title">Networking</div>
      <table class="grid">
        <tr><th>Name</th><th>Bridge</th><th>State</th></tr>
        ${nets.map((n) => `
          <tr class="row-open" tabindex="0" data-dash-net="${esc(n.name)}" data-dash-conn="${selectedConn}"><td>${esc(n.name)}</td><td>${esc(n.bridge)}</td>
          <td><span class="state ${n.active ? "on" : ""}">${n.active ? "Active" : "Inactive"}</span></td></tr>`).join("") || `<tr><td colspan="3" class="empty">No networks.</td></tr>`}
      </table>
    </div>

    <div class="dash-section">
      <div class="section-title">Storage Pools</div>
      ${pools.map((p) => {
        const used = p.allocation ?? 0;
        const cap = p.capacity ?? 0;
        const usedPct = cap > 0 ? Math.max(0, Math.min(100, (used / cap) * 100)) : 0;
        return `
        <div class="storage-card ${p.active ? "ok" : ""}" data-open tabindex="0" data-dash-pool="${esc(p.name)}" data-dash-conn="${selectedConn}">
          <div class="storage-card-head">
            <span class="storage-card-icon">${ic.storage()}</span>
            <span class="storage-card-title">${esc(p.name)}</span>
            <span class="badge ${p.active ? "on" : ""}">${p.active ? "Active" : "Inactive"}</span>
          </div>
          <div class="metric-value">${usedPct.toFixed(usedPct >= 10 ? 0 : 1)}%</div>
          <div class="meter"><div class="meter-fill ${usedPct > 85 ? "hot" : ""}" style="width:${usedPct.toFixed(1)}%"></div></div>
          <div class="metric-foot">
            <span>${fmtBytes(used)} used</span>
            <span>${fmtBytes(cap)} total</span>
          </div>
        </div>`;
      }).join("") || `<div class="empty">No pools.</div>`}
    </div>
    `);
    bindDashOpeners(body);
  } catch (e) {
    setHtml(body, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

/// Dashboard pool cards and network rows open their management page. Shared
/// by the datacenter and server dashboards, which render the same widgets.
function bindDashOpeners(root: HTMLElement) {
  root.querySelectorAll("[data-dash-pool]").forEach((el: Element) => {
    const d = (el as HTMLElement).dataset;
    (el as HTMLElement).onclick = () => selectPoolRow(d.dashConn!, d.dashPool!);
  });
  root.querySelectorAll("[data-dash-net]").forEach((el: Element) => {
    const d = (el as HTMLElement).dataset;
    (el as HTMLElement).onclick = () => selectNetRow(d.dashConn!, d.dashNet!);
  });
}

/// Per-network firewall editor: rules and NAT port forwards, enforced by the
/// daemon via nftables. Replaces the old host-wide nwfilter preset list.
async function loadFilters() {
  if (!selectedConn) return;
  const list = $("#filters-list");
  if (!list) return;
  try {
    const nets = await api.get<NetworkSummary[]>(`/proxy/${selectedConn}/networks`);
    setHtml(list, `
      <table class="grid">
        <tr><th>Network</th><th>Type</th><th>Subnet</th><th class="col-actions"></th></tr>
        ${nets.map((n) => `
          <tr>
            <td class="tname">${esc(n.name)}</td>
            <td>${esc(n.forward === "bridge" ? "Bridge" : n.forward === "nat" ? "NAT" : "Isolated")}</td>
            <td>${n.ip ? esc(n.ip) : "—"}</td>
            <td class="col-actions">
              <div class="row-actions">
                <button class="btn" data-fwedit="${esc(n.name)}">${ic.firewall(14)} Rules</button>
              </div>
            </td>
          </tr>`).join("") || `<tr><td colspan="4" class="empty">No networks.</td></tr>`}
      </table>
      <div class="actions"><button class="btn al-open" id="fw-tab-logs">${ic.logs(14)} Access logs</button></div>`);
    const conn = selectedConn;
    $("#fw-tab-logs", list).onclick = () => openAccessLogs(conn);
    list.querySelectorAll("[data-fwedit]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => openFirewallEditor((b as HTMLElement).dataset.fwedit!);
    });
  } catch (e) {
    setHtml(list, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

/// Access log: every connection this server's guests made, two weeks back.
/// `vm` pre-selects one guest (the VM Network tab); from the Firewall page it
/// starts on every VM with a filter to pick one.
function openAccessLogs(conn: string, vm = "") {
  let f: LogFilter = defaultFilter(vm);
  let vms: string[] = [];
  const el = openModal(`Access logs${vm ? ` — ${esc(vm)}` : ""}`, `<div id="al-body">${skeleton("table")}</div>`, "");
  el.querySelector(".modal")?.classList.add("wide", "al-modal");
  void api.get<{ vms: string[] }>(`/access-logs/vms?conn=${encodeURIComponent(conn)}`)
    .then((r) => { vms = r.vms; paint(null); }).catch(() => {});
  let data: { total: number; rows?: AccessRow[]; groups?: AccessGroup[] } | null = null;
  let gen = 0;
  // Live tail: re-read the newest page every couple of seconds while the
  // dialog is open. New rows land within the web server's 5 s pull.
  let live = false;
  let liveTimer = 0;
  const stopLive = () => { live = false; window.clearInterval(liveTimer); };
  const startLive = () => {
    live = true;
    window.clearInterval(liveTimer);
    liveTimer = window.setInterval(() => {
      if (!el.isConnected) return stopLive();
      void load();
    }, 2000);
  };

  const load = async () => {
    const mine = ++gen;
    try {
      const r = await api.get<typeof data>(`/access-logs?${logQuery(conn, f, Date.now())}`);
      if (mine !== gen) return; // a newer filter won
      data = r;
    } catch (e) {
      if (mine !== gen) return;
      data = null;
      return paint((e as Error).message);
    }
    paint(null);
  };

  const opt = (list: [string, string][], sel: string) =>
    list.map(([v, l]) => `<option value="${esc(v)}" ${v === sel ? "selected" : ""}>${esc(l)}</option>`).join("");
  const th = (col: string, label: string) =>
    `<th class="al-sort" data-al-sort="${col}" tabindex="0">${label}${sortMark(f, col)}</th>`;

  function paint(error: string | null) {
    const body = $("#al-body", el) as HTMLElement | null;
    if (!body) return;
    const vmList = [...new Set([...vms, ...(f.vm ? [f.vm] : [])])];
    const total = data?.total ?? 0;
    const pages = Math.max(1, Math.ceil(total / PAGE));
    let table = "";
    if (error) {
      table = `<div class="empty">${esc(error)}</div>`;
    } else if (!data) {
      table = skeleton("table");
    } else if (f.group) {
      table = `<table class="grid al-table">
        <tr>${th("key", esc(GROUP_LABEL[f.group].replace(/^(Most accessed |Most used |Busiest )/, "")))}${th("count", "Connections")}<th>VMs</th>${th("time", "Last")}</tr>
        ${(data.groups ?? []).map((g) => `
          <tr class="row-open" tabindex="0" data-al-drill="${esc(g.key)}">
            <td>${esc(g.key || "(no name)")}</td><td>${g.count}</td>
            <td>${g.vms}</td><td>${fmtWhen(g.last_ms)}</td></tr>`).join("")
          || `<tr><td colspan="4" class="empty">Nothing logged for this filter.</td></tr>`}
      </table>`;
    } else {
      table = `<table class="grid al-table">
        <tr>${th("time", "Time")}${f.vm ? "" : th("vm", "VM")}<th>Dir</th>${th("proto", "Proto")}${th("domain", "Domain")}${th("ip", "Remote")}${th("port", "Port")}</tr>
        ${(data.rows ?? []).map((r) => `
          <tr>
            <td>${fmtWhen(r.opened_ms)}</td>
            ${f.vm ? "" : `<td>${esc(r.vm ?? "—")}</td>`}
            <td class="al-dir ${r.dir === "in" ? "al-in" : "al-out"}" title="${r.dir === "in" ? "Someone reached this guest" : "The guest opened this connection"}">${r.dir === "in" ? "in" : "out"}</td>
            <td>${r.kind === "dns" ? "dns" : esc(r.proto)}</td>
            <td class="al-domain">${esc(r.domain ?? "—")}</td>
            <td><code>${esc(r.dst)}</code></td>
            <td>${r.dport || "—"}</td>
          </tr>`).join("")
          || `<tr><td colspan="${f.vm ? 6 : 7}" class="empty">Nothing logged for this filter.</td></tr>`}
      </table>`;
    }
    const restore = holdFocus(body, "[data-al-sort], [data-al-drill], select, input");
    setHtml(body, `
      <div class="al-filters">
        <label class="field"><span>VM</span><select id="al-vm">${opt([["", "All VMs"], ...vmList.map((v): [string, string] => [v, v])], f.vm)}</select></label>
        <label class="field"><span>Show</span><select id="al-group">${opt(Object.entries(GROUP_LABEL) as [string, string][], f.group)}</select></label>
        <label class="field"><span>Direction</span><select id="al-direction">${opt(Object.entries(DIR_LABEL) as [string, string][], f.direction)}</select></label>
        <label class="field"><span>Protocol</span><select id="al-proto">${opt([["", "Any"], ["tcp", "TCP"], ["udp", "UDP"], ["icmp", "ICMP"]], f.proto)}</select></label>
        <label class="field"><span>Port</span><input id="al-port" value="${esc(f.port)}" placeholder="any" inputmode="numeric"></label>
        <label class="field al-grow"><span>Domain or IP</span><input id="al-q" value="${esc(f.q)}" placeholder="github.com, 140.82."></label>
        <label class="field"><span>Period</span><select id="al-range">${opt([["1h", "Last hour"], ["24h", "Last 24 h"], ["7d", "Last 7 days"], ["14d", "Last 14 days"]], f.range)}</select></label>
        <label class="field"><span>Records</span><select id="al-kind">${opt(Object.entries(KIND_LABEL) as [string, string][], f.kind)}</select></label>
        <button class="btn al-live ${live ? "on" : ""}" id="al-live" title="Show new entries as they arrive">${ic.circleDot(14)} Live</button>
      </div>
      ${table}
      <div class="al-foot">
        <span class="dsub">${total} ${f.group ? "groups" : "entries"} · kept 14 days; oldest removed first if disk space runs low</span>
        <span class="al-pager">
          <button class="btn icon" id="al-prev" ${f.page ? "" : "disabled"} title="Newer page">${ic.back()}</button>
          <span>${f.page + 1} / ${pages}</span>
          <button class="btn icon" id="al-next" ${f.page + 1 < pages ? "" : "disabled"} title="Older page">${ic.forward()}</button>
          <button class="btn icon" id="al-reload" title="Refresh">${ic.reboot()}</button>
        </span>
      </div>`);
    restore();

    const set = (next: LogFilter) => {
      f = next;
      if (live && !liveApplies(f)) stopLive();
      void load();
    };
    const sel = (id: string, fn: (v: string) => LogFilter) =>
      ($(id, body) as HTMLSelectElement).addEventListener("change", (e) => set(fn((e.target as HTMLSelectElement).value)));
    sel("#al-vm", (v) => ({ ...f, vm: v, page: 0 }));
    sel("#al-group", (v) => withGroup(f, v as Group));
    sel("#al-proto", (v) => ({ ...f, proto: v, page: 0 }));
    sel("#al-direction", (v) => ({ ...f, direction: v as FlowDir, page: 0 }));
    sel("#al-range", (v) => ({ ...f, range: v, page: 0 }));
    sel("#al-kind", (v) => ({ ...f, kind: v as Kind, page: 0 }));
    ($("#al-live", body) as HTMLButtonElement).onclick = () => {
      if (live) { stopLive(); paint(null); return; }
      // Live means newest first: switch to that view, keeping the filters.
      f = forLive(f);
      startLive();
      void load();
    };
    // Typing filters as you go, after a pause, instead of on every key.
    let timer = 0;
    for (const [id, key] of [["#al-q", "q"], ["#al-port", "port"]] as const) {
      ($(id, body) as HTMLInputElement).oninput = (e) => {
        window.clearTimeout(timer);
        const v = (e.target as HTMLInputElement).value;
        timer = window.setTimeout(() => set({ ...f, [key]: v, page: 0 }), 350);
      };
    }
    body.querySelectorAll<HTMLElement>("[data-al-sort]").forEach((h) => {
      h.onclick = () => set(toggleSort(f, h.dataset.alSort!));
      h.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); h.click(); } };
    });
    body.querySelectorAll<HTMLElement>("[data-al-drill]").forEach((r) => {
      r.onclick = () => set(drillDown(f, r.dataset.alDrill!));
    });
    ($("#al-prev", body) as HTMLButtonElement).onclick = () => set({ ...f, page: Math.max(0, f.page - 1) });
    ($("#al-next", body) as HTMLButtonElement).onclick = () => set({ ...f, page: f.page + 1 });
    ($("#al-reload", body) as HTMLButtonElement).onclick = () => void load();
  }
  paint(null);
  void load();
}

interface AccessRow {
  opened_ms: number; closed_ms: number; vm: string | null; proto: string; dst: string; dport: number;
  domain: string | null; dir: FlowDir;
  kind: "dns" | "connection";
}
interface AccessGroup { key: string; count: number; vms: number; last_ms: number }

/// Rules + port-forward editor for one network.
/// Shared firewall-editor helpers: option lists, row templates, and reading
/// rows back from the DOM before a re-render.
const FW_DIRS = ["in", "out", "forward"];
const FW_ACTIONS = ["accept", "drop", "reject"];
const FW_PROTOS = ["tcp", "udp", "icmp", "any"];
const FW_ACTION_LABELS: Record<string, string> = { accept: "Accept", drop: "Drop", reject: "Reject" };

function fwOpt(list: string[], sel: string, labels?: Record<string, string>): string {
  return list.map((v) => `<option value="${v}" ${v === sel ? "selected" : ""}>${esc(labels?.[v] ?? v)}</option>`).join("");
}

/// IP addresses worth suggesting in the firewall's source/dest fields: every
/// network's gateway, its DHCP endpoints, and each static allocation. Fetched
/// once per editor open and read lazily by the suggesters, so the list can
/// arrive after the rule table has already painted.
async function collectIpCandidates(conn: string): Promise<string[]> {
  const nets = await api.get<NetworkSummary[]>(`/proxy/${conn}/networks`).catch(() => []);
  if (!Array.isArray(nets)) return [];
  // Each network contributes the addresses a rule might name; the callback
  // returns them rather than adding to an outer Set, so every arm of it
  // returns a value (array-callback-return).
  const found = await Promise.all(nets.map(async (n) => {
    const base = `/proxy/${conn}/networks/${encodeURIComponent(n.name)}`;
    const [ipam, hosts] = await Promise.all([
      api.get<IpamInfo>(`${base}/ipam`).catch(() => null),
      api.get<{ hosts: IpamHost[] }>(`${base}/hosts`).catch(() => null),
    ]);
    const addrs = (hosts?.hosts ?? []).map((h) => h.ip);
    if (ipam) addrs.push(ipam.gateway, ipam.range_start, ipam.range_end);
    return addrs;
  }));
  return [...new Set(found.flat())].sort();
}

/// Offer IP suggestions on a rule table's source/dest inputs. Called after
/// every render: the inputs are recreated each time, so listeners never stack.
function attachRuleIpSuggest(root: Element, candidates: () => string[]) {
  root.querySelectorAll("[data-r-src], [data-r-dst]").forEach((n: Element) => {
    attachSuggest(n as HTMLInputElement, candidates);
  });
}

function fwRuleRow(r: FirewallRule): string {
  return `
    <tr>
      <td><select data-r-dir>${fwOpt(FW_DIRS, r.direction)}</select></td>
      <td><select data-r-action>${fwOpt(FW_ACTIONS, r.action, FW_ACTION_LABELS)}</select></td>
      <td><select data-r-proto>${fwOpt(FW_PROTOS, r.proto)}</select></td>
      <td><input data-r-src placeholder="any" value="${esc(r.src_ip)}"></td>
      <td><input data-r-dst placeholder="any" value="${esc(r.dst_ip)}"></td>
      <td><input data-r-sport placeholder="any" value="${esc(r.src_port)}"></td>
      <td><input data-r-dport placeholder="any" value="${esc(r.dst_port)}"></td>
      <td><button class="btn danger icon" data-r-del title="Remove">${ic.trash()}</button></td>
    </tr>`;
}

function fwFwdRow(f: FirewallForward): string {
  return `
    <tr>
      <td><input data-f-wanip placeholder="0.0.0.0 = any" value="${f.wan_ip === "0.0.0.0" ? "" : esc(f.wan_ip)}"></td>
      <td><select data-f-proto>${fwOpt(["tcp", "udp"], f.proto)}</select></td>
      <td><input data-f-wstart type="number" min="1" max="65535" value="${f.wan_start}"></td>
      <td><input data-f-wend type="number" min="1" max="65535" value="${f.wan_end}"></td>
      <td><input data-f-lanip placeholder="192.168.x.x" value="${esc(f.lan_ip)}"></td>
      <td><input data-f-lstart type="number" min="1" max="65535" value="${f.lan_start}"></td>
      <td><input data-f-lend type="number" min="1" max="65535" value="${f.lan_end}"></td>
      <td><button class="btn danger icon" data-f-del title="Remove">${ic.trash()}</button></td>
    </tr>`;
}

function blankRule(): FirewallRule {
  return { direction: "in", action: "accept", proto: "any", src_ip: "", dst_ip: "", src_port: "", dst_port: "" };
}

function blankPolicy(): FirewallPolicy {
  return { in: "accept", out: "accept", forward: "accept" };
}

/// Three dropdowns for the per-direction default policies, in one row.
function fwPolicyRow(p: FirewallPolicy): string {
  return `
    <div class="fw-policy">
      <label class="field"><span>In</span><select data-pol-in>${fwOpt(FW_ACTIONS, p.in, FW_ACTION_LABELS)}</select></label>
      <label class="field"><span>Out</span><select data-pol-out>${fwOpt(FW_ACTIONS, p.out, FW_ACTION_LABELS)}</select></label>
      <label class="field"><span>Forward</span><select data-pol-forward>${fwOpt(FW_ACTIONS, p.forward, FW_ACTION_LABELS)}</select></label>
    </div>`;
}

function readFwPolicy(root: Element): FirewallPolicy {
  const v = (sel: string) => (root.querySelector(sel) as HTMLSelectElement)?.value as FirewallPolicy["in"];
  return {
    in: v("[data-pol-in]") ?? "accept",
    out: v("[data-pol-out]") ?? "accept",
    forward: v("[data-pol-forward]") ?? "accept",
  };
}

function readFwRuleRows(root: Element): FirewallRule[] {
  return [...root.querySelectorAll("[data-r-dir]")].map((s) => {
    const tr = (s as HTMLElement).closest("tr")!;
    return {
      direction: (tr.querySelector("[data-r-dir]") as HTMLSelectElement).value as FirewallRule["direction"],
      action: (tr.querySelector("[data-r-action]") as HTMLSelectElement).value as FirewallRule["action"],
      proto: (tr.querySelector("[data-r-proto]") as HTMLSelectElement).value as FirewallRule["proto"],
      src_ip: (tr.querySelector("[data-r-src]") as HTMLInputElement).value,
      dst_ip: (tr.querySelector("[data-r-dst]") as HTMLInputElement).value,
      src_port: (tr.querySelector("[data-r-sport]") as HTMLInputElement).value,
      dst_port: (tr.querySelector("[data-r-dport]") as HTMLInputElement).value,
    };
  });
}

function readFwFwdRows(root: Element): FirewallForward[] {
  return [...root.querySelectorAll("[data-f-lanip]")].map((s) => {
    const tr = (s as HTMLElement).closest("tr")!;
    return {
      proto: (tr.querySelector("[data-f-proto]") as HTMLSelectElement).value as FirewallForward["proto"],
      wan_ip: (tr.querySelector("[data-f-wanip]") as HTMLInputElement).value || "0.0.0.0",
      wan_start: +(tr.querySelector("[data-f-wstart]") as HTMLInputElement).value,
      wan_end: +(tr.querySelector("[data-f-wend]") as HTMLInputElement).value,
      lan_ip: (tr.querySelector("[data-f-lanip]") as HTMLInputElement).value,
      lan_start: +(tr.querySelector("[data-f-lstart]") as HTMLInputElement).value,
      lan_end: +(tr.querySelector("[data-f-lend]") as HTMLInputElement).value,
    };
  });
}

/// Network firewall editor: rules and NAT port forwards, in a wide modal.
function openFirewallEditor(netName: string) {
  if (!selectedConn) return;
  const conn = selectedConn;
  let rules: FirewallRule[] = [];
  let forwards: FirewallForward[] = [];
  let policy: FirewallPolicy = blankPolicy();
  // Every address this daemon knows about, for the source/dest fields.
  // Filled after the modal paints; the suggesters read it lazily per keystroke.
  let ipCandidates: string[] = [];
  void collectIpCandidates(conn).then((ips) => { ipCandidates = ips; });

  const el = openModal(
    `Firewall — ${esc(netName)}`,
    `<div id="fw-body">${skeleton("table")}</div>`,
    `<button class="btn" id="fw-cancel">Cancel</button>
     <button class="btn primary" id="fw-save">Apply</button>`,
  );
  el.querySelector(".modal")?.classList.add("wide");

  const read = () => {
    rules = readFwRuleRows(el);
    forwards = readFwFwdRows(el);
    policy = readFwPolicy(el);
  };

  function render() {
    setHtml($("#fw-body", el), `
      <div class="section-title fw-cat">Default Policies</div>
      ${fwPolicyRow(policy)}

      <div class="section-title fw-cat">Rules</div>
      <table class="grid">
        <tr><th>Direction</th><th>Action</th><th>Proto</th><th>Source IP</th><th>Dest IP</th><th>Source Port</th><th>Dest Port</th><th></th></tr>
        ${rules.map(fwRuleRow).join("") || `<tr><td colspan="8" class="empty">No rules — traffic is accepted.</td></tr>`}
      </table>
      <div class="actions">
        <button class="btn" id="fw-add-rule">${ic.plus(14)} Add rule</button>
        <button class="btn al-open" id="fw-logs">${ic.logs(14)} Access logs</button>
      </div>

      <div class="section-title fw-cat" style="margin-top:18px">NAT Port Forwards</div>
      <table class="grid">
        <tr><th>WAN</th><th>Proto</th><th>WAN Start</th><th>WAN End</th><th>Internal IP</th><th>Start</th><th>End</th><th></th></tr>
        ${forwards.map(fwFwdRow).join("") || `<tr><td colspan="8" class="empty">No port forwards.</td></tr>`}
      </table>
      <div class="actions"><button class="btn" id="fw-add-fwd">${ic.plus(14)} Add forward</button></div>`);

    // Re-attached every render: render() rebuilds these inputs from scratch.
    attachRuleIpSuggest(el, () => ipCandidates);

    $("#fw-logs", el).onclick = () => openAccessLogs(conn);
    $("#fw-add-rule", el).onclick = () => {
      read();
      rules.push(blankRule());
      render();
    };
    $("#fw-add-fwd", el).onclick = () => {
      read();
      forwards.push({ wan_ip: "0.0.0.0", wan_start: 1, wan_end: 1, lan_ip: "", lan_start: 1, lan_end: 1, proto: "tcp" });
      render();
    };
    el.querySelectorAll("[data-r-del]").forEach((b) => {
      (b as HTMLElement).onclick = () => {
        read();
        const tr = (b as HTMLElement).closest("tr")!;
        // Index among the DATA rows only: tbody children include the header
        // <tr>, so counting them would splice the wrong rule (and the first
        // rule would never delete).
        const rows = [...el.querySelectorAll("[data-r-dir]")].map((s) => (s as HTMLElement).closest("tr")!);
        rules.splice(rows.indexOf(tr), 1);
        render();
      };
    });
    el.querySelectorAll("[data-f-del]").forEach((b) => {
      (b as HTMLElement).onclick = () => {
        read();
        const tr = (b as HTMLElement).closest("tr")!;
        const rows = [...el.querySelectorAll("[data-f-lanip]")].map((s) => (s as HTMLElement).closest("tr")!);
        forwards.splice(rows.indexOf(tr), 1);
        render();
      };
    });
  }

  api.get<{ rules: FirewallRule[]; forwards: FirewallForward[]; policy: FirewallPolicy }>(
    `/proxy/${selectedConn}/networks/${encodeURIComponent(netName)}/firewall`)
    .then((fw) => {
      rules = fw.rules ?? [];
      forwards = fw.forwards ?? [];
      policy = fw.policy ?? blankPolicy();
      render();
    })
    .catch((e) => toast((e as Error).message, "bad"));

  $("#fw-cancel", el).onclick = () => el.remove();
  $("#fw-save", el).onclick = async () => {
    read();
    try {
      await api.put(`/proxy/${selectedConn}/networks/${encodeURIComponent(netName)}/firewall`,
        { rules, forwards, policy });
      toast("Firewall applied");
      el.remove();
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };
}

async function loadVms() {
  if (!selectedConn) return;
  try {
    const vms = await api.get<DomainSummary[]>(`/proxy/${selectedConn}/domains`);
    const list = $("#vms-list");
    if (!list) return;
    setHtml(list, vms.map((v) => `
      <div class="storage-card ${v.active ? "ok" : ""}" data-open tabindex="0" data-vm-open="${esc(v.name)}">
        <div class="storage-card-head">
          <span class="storage-card-icon">${ic.vm()}</span>
          <span class="storage-card-title">${esc(v.title || v.name)}</span>
          <span class="badge ${v.active ? "on" : ""}">${v.active ? "Active" : "Inactive"}</span>
        </div>
        <div class="specs">
          <div class="spec"><b>vCPUs</b><span>${v.vcpus || "—"}</span></div>
          <div class="spec"><b>Memory</b><span>${v.memory_mb ? fmtBytes(v.memory_mb * 1024 * 1024) : "—"}</span></div>
        </div>
        <div class="storage-card-actions net-actions">
          <button class="btn icon ${v.active ? "on" : ""}" data-vm-toggle="${esc(v.name)}" data-vm-action="${v.active ? "stop" : "start"}" title="${v.active ? "Shutdown" : "Start"}">${v.active ? ic.stop() : ic.start()}</button>
          <button class="btn icon" data-vm-console="${esc(v.name)}" title="Console">${ic.vm()}</button>
          <button class="btn icon" data-vm-edit="${esc(v.name)}" title="Edit hardware">${ic.edit()}</button>
          <button class="btn danger icon" data-vm-del="${esc(v.name)}" title="Delete">${ic.trash()}</button>
        </div>
      </div>`).join("") || emptyState("VMs", "Create VM", "vm-create-empty"));
    const createBtn = $("#vm-create-empty");
    if (createBtn) createBtn.onclick = () => openCreateVm();
    // The whole card opens the guest; action buttons stop propagation.
    list.querySelectorAll("[data-vm-open]").forEach((card: Element) => {
      (card as HTMLElement).onclick = () => {
        selectedVm = (card as HTMLElement).dataset.vmOpen!;
        vmTab = "summary";
        renderVm();
      };
    });
    list.querySelectorAll("[data-vm-toggle]").forEach((b: Element) => {
      (b as HTMLElement).onclick = async (e) => {
        e.stopPropagation();
        const n = (b as HTMLElement).dataset.vmToggle!;
        const start = (b as HTMLElement).dataset.vmAction === "start";
        await act(`/proxy/${selectedConn}/domains/${encodeURIComponent(n)}/${start ? "start" : "shutdown"}`, "POST");
        loadVms();
      };
    });
    // Jump straight to the console tab for that guest.
    list.querySelectorAll("[data-vm-console]").forEach((b: Element) => {
      (b as HTMLElement).onclick = (e) => {
        e.stopPropagation();
        leaveCurrentView();
        selectedVm = (b as HTMLElement).dataset.vmConsole!;
        vmTab = "console";
        renderTree();
        void renderVm();
      };
    });
    list.querySelectorAll("[data-vm-edit]").forEach((b: Element) => {
      (b as HTMLElement).onclick = (e) => {
        e.stopPropagation();
        void openHardwareEditor({ vm: (b as HTMLElement).dataset.vmEdit! });
      };
    });
    list.querySelectorAll("[data-vm-del]").forEach((b: Element) => {
      (b as HTMLElement).onclick = (e) => {
        e.stopPropagation();
        void deleteVm((b as HTMLElement).dataset.vmDel!);
      };
    });
  } catch (e) {
    const list = $("#vms-list");
    if (list) setHtml(list, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

async function loadNetworks() {
  if (!selectedConn) return;
  try {
    const nets = await api.get<NetworkSummary[]>(`/proxy/${selectedConn}/networks`);
    const list = $("#nets-list");
    if (!list) return;
    const typeLabel = (n: NetworkSummary) => {
      if (n.forward === "bridge") return "Bridge";
      if (n.forward === "nat") return "NAT";
      return "Isolated";
    };
    setHtml(list, nets.map((n) => `
      <div class="storage-card net-card ${n.active ? "ok" : ""}" data-open tabindex="0" data-net-open="${esc(n.name)}">
        <div class="storage-card-head">
          <span class="storage-card-icon">${ic.network()}</span>
          <span class="storage-card-title">${esc(n.name)}</span>
          <span class="badge">${esc(typeLabel(n))}</span>
          <span class="badge ${n.active ? "on" : ""}">${n.active ? "Active" : "Inactive"}</span>
        </div>
        <div class="specs">
          <div class="spec"><b>Bridge</b><span>${esc(n.bridge || "—")}</span></div>
          <div class="spec"><b>Subnet</b><span>${n.ip ? esc(n.ip) : "—"}</span></div>
        </div>
        <div class="storage-card-actions net-actions">
          <button class="btn icon ${n.active ? "on" : ""}" data-net-toggle="${esc(n.name)}" data-net-action="${n.active ? "stop" : "start"}" title="${n.active ? "Stop" : "Start"}">${n.active ? ic.stop() : ic.start()}</button>
          <button class="btn icon ${n.autostart ? "on" : ""}" data-net-autostart="${esc(n.name)}" title="${n.autostart ? "Autostart on" : "Autostart off"}">${ic.autostart()}</button>
          <button class="btn icon" data-net-edit="${esc(n.name)}" title="Edit">${ic.edit()}</button>
          <button class="btn danger icon" data-delnet="${esc(n.name)}" title="Delete">${ic.trash()}</button>
        </div>
      </div>`).join("") || emptyState("networks", "Create Network", "net-create-empty"));
    const createBtn = $("#net-create-empty");
    if (createBtn) createBtn.onclick = () => openCreateNetwork();
    // The whole card opens the network's page; its buttons stop propagation.
    list.querySelectorAll("[data-net-open]").forEach((card: Element) => {
      (card as HTMLElement).onclick = (e) => {
        if ((e.target as Element).closest?.("button")) return;
        selectNetRow(selectedConn!, (card as HTMLElement).dataset.netOpen!);
      };
    });
    list.querySelectorAll("[data-net-toggle]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => netAction(
        (b as HTMLElement).dataset.netAction === "start" ? "start" : "stop",
        (b as HTMLElement).dataset.netToggle!,
      );
    });
    list.querySelectorAll("[data-net-autostart]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => toggleAutostart((b as HTMLElement).dataset.netAutostart!);
    });
    list.querySelectorAll("[data-net-edit]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => openEditNetwork((b as HTMLElement).dataset.netEdit!);
    });
    list.querySelectorAll("[data-delnet]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => deleteNetwork((b as HTMLElement).dataset.delnet!);
    });
  } catch (e) {
    const list = $("#nets-list");
    if (list) setHtml(list, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

async function netAction(action: "start" | "stop", name: string) {
  if (!selectedConn) return;
  try {
    await api.post(`/proxy/${selectedConn}/networks/${encodeURIComponent(name)}/${action}`);
    await loadNetworks();
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}

async function toggleAutostart(name: string) {
  if (!selectedConn) return;
  const net = (await api.get<NetworkSummary[]>(`/proxy/${selectedConn}/networks`))
    .find((n) => n.name === name);
  if (!net) return;
  try {
    await api.post(`/proxy/${selectedConn}/networks/${encodeURIComponent(name)}/autostart`,
      { autostart: !net.autostart });
    await loadNetworks();
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}

async function openEditNetwork(name: string) {
  if (!selectedConn) return;
  let xml = "";
  try {
    xml = (await api.get<{ xml: string }>(`/proxy/${selectedConn}/networks/${encodeURIComponent(name)}/xml`)).xml;
  } catch (e) {
    return toast((e as Error).message, "bad");
  }
  openNetworkForm(networkFormFromXml(xml), networkUuid(xml));
}

/// `conn` defaults to the selected server. The admin page passes its own
/// (with an owner and a callback), so creating a network for a user happens
/// over the admin page instead of navigating away from it.
function openCreateNetwork(conn = selectedConn, forUser?: { owner: string; after: () => void }) {
  openNetworkForm(
    { name: "", forward: "nat", bridge: "", ip: "", dhcp_start: "", dhcp_end: "", dns: "", hosts: [] },
    "",
    conn,
    forUser,
  );
}

/// One network form for create and edit. Fields appear and disappear with the
/// forward mode: bridge mode offers a bridge picker and no IP/DHCP/DNS (the
/// bridge owns addressing); NAT offers IP, DHCP and DNS forwarders; isolated
/// offers IP/DHCP but no bridge.
function openNetworkForm(
  initial: NetworkForm,
  uuid: string,
  conn: string | null = selectedConn,
  forUser?: { owner: string; after: () => void },
) {
  if (!conn) return;
  const el = openModal(
    uuid ? `Edit network — ${esc(initial.name)}` : "Create network",
    `<div class="grid2">
      <label class="field"><span>Name</span><input id="net-name" value="${esc(initial.name)}"></label>
      <label class="field"><span>Mode</span><select id="net-mode">
        <option value="nat" ${initial.forward === "nat" ? "selected" : ""}>NAT</option>
        <option value="bridge" ${initial.forward === "bridge" ? "selected" : ""}>Bridge</option>
        <option value="none" ${initial.forward === "none" ? "selected" : ""}>Isolated</option>
      </select></label>
      <label class="field" data-net-field="bridge"><span>Bridge</span><select id="net-bridge">
        <option value="">Select a bridge…</option>
        ${initial.bridge ? `<option value="${esc(initial.bridge)}" selected>${esc(initial.bridge)}</option>` : ""}
      </select></label>
      <label class="field" data-net-field="ip"><span>Network (CIDR, optional)</span><input id="net-ip" placeholder="192.168.100.1/24" value="${esc(initial.ip)}"></label>
      <label class="field" data-net-field="dhcp-start"><span>DHCP start</span><input id="net-dhcp-start" placeholder="192.168.100.10" value="${esc(initial.dhcp_start)}"></label>
      <label class="field" data-net-field="dhcp-end"><span>DHCP end</span><input id="net-dhcp-end" placeholder="192.168.100.200" value="${esc(initial.dhcp_end)}"></label>
      <label class="field" data-net-field="dns"><span>DNS servers (NAT, comma separated)</span><input id="net-dns" placeholder="1.1.1.1, 8.8.8.8" value="${esc(initial.dns)}"></label>
    </div>`,
    `<button class="btn primary" id="net-save">${uuid ? "Save" : "Create"}</button>`,
  );

  const mode = () => ($("#net-mode", el) as HTMLSelectElement).value as NetworkForm["forward"];
  const sync = () => {
    const m = mode();
    el.querySelectorAll("[data-net-field]").forEach((f: Element) => {
      const field = (f as HTMLElement).dataset.netField!;
      let show: boolean;
      if (m === "bridge") {
        show = field === "bridge";
      } else if (m === "nat") {
        show = field !== "bridge";
      } else {
        show = field !== "bridge" && field !== "dns";
      }
      (f as HTMLElement).hidden = !show;
    });
  };
  $("#net-mode", el).addEventListener("change", sync);

  // Bridge mode picks from the host's real bridge devices.
  const bridgeSelect = $("#net-bridge", el) as HTMLSelectElement;
  api.get<string[]>(`/proxy/${conn}/networks/bridges`)
    .then((bridges) => {
      const current = initial.bridge;
      bridgeSelect.replaceChildren();
      const ph = document.createElement("option");
      ph.value = "";
      ph.textContent = "Select a bridge…";
      bridgeSelect.append(ph);
      for (const b of bridges) {
        const o = document.createElement("option");
        o.value = b;
        o.textContent = b;
        o.selected = b === current;
        bridgeSelect.append(o);
      }
      enhanceSelects(bridgeSelect);
    })
    .catch(() => {}); // no bridges: the picker shows just the placeholder

  $("#net-save", el).onclick = async () => {
    const form: NetworkForm = {
      name: $("#net-name", el).value,
      forward: mode(),
      bridge: (bridgeSelect as HTMLSelectElement).value,
      ip: $("#net-ip", el).value,
      dhcp_start: $("#net-dhcp-start", el).value,
      dhcp_end: $("#net-dhcp-end", el).value,
      dns: $("#net-dns", el).value,
      hosts: initial.hosts ?? [],
    };
    if (!form.name) return toast("Name is required", "bad");
    if (form.forward === "bridge" && !form.bridge) {
      return toast("Select a bridge", "bad");
    }
    el.remove();
    const xml = networkXml(form, uuid);
    const owner = uuid ? null : forUser?.owner ?? null;
    void runTask(`${uuid ? "Update" : "Create"} network ${form.name}`, async () => {
      if (uuid) await api.putXml(`/proxy/${conn}/networks/${encodeURIComponent(initial.name)}/xml`, xml);
      else await api.postXml(`/proxy/${conn}/networks`, xml);
      if (owner) await assignOwner(conn, "network", form.name, owner);
    }, { done: `Network ${form.name} ${uuid ? "updated" : "created"}`, notify: toast })
      .then(() => {
        void refreshTreeVms();
        if (forUser) forUser.after();
        else if (selectedConn === conn && !selectedVm) void renderConn();
      });
  };

  sync();
}

async function loadPools() {
  if (!selectedConn) return;
  try {
    const pools = await api.get<PoolSummary[]>(`/proxy/${selectedConn}/pools`);
    const list = $("#pools-list");
    if (!list) return;
    const names = pools.map((p) => p.name);
    const shapeChanged = renderedPools?.join("|") !== names.join("|");
    if (shapeChanged) {
      renderedPools = names;
      setHtml(list, pools.map((p) => {
        const used = p.allocation ?? 0;
        const cap = p.capacity ?? 0;
        const usedPct = cap > 0 ? Math.max(0, Math.min(100, (used / cap) * 100)) : 0;
        return `
      <div class="storage-card ${p.active ? "ok" : ""}" data-open tabindex="0" data-pool-open="${esc(p.name)}" data-pool-card="${cssId(p.name)}">
        <div class="storage-card-head">
          <span class="storage-card-icon">${ic.storage()}</span>
          <span class="storage-card-title">${esc(p.name)}</span>
          <span class="badge" data-pool-active>${p.active ? "Active" : "Inactive"}</span>
        </div>
        <div class="metric-value" data-pool-pct>${usedPct.toFixed(usedPct >= 10 ? 0 : 1)}%</div>
        <div class="meter"><div class="meter-fill" data-pool-fill style="width:${usedPct.toFixed(1)}%"></div></div>
        <div class="metric-foot">
          <span data-pool-used>${fmtBytes(used)} used</span>
          <span data-pool-free>${fmtBytes(p.available)} free</span>
          <span data-pool-cap>${fmtBytes(cap)} total</span>
        </div>
        <div class="storage-card-actions">
          <button class="btn" data-vols="${esc(p.name)}">${ic.disk(14)} Volumes</button>
          <button class="btn" data-upload="${esc(p.name)}">${ic.upload(14)} Upload ISO</button>
          <button class="btn danger icon" data-delpool="${esc(p.name)}">${ic.trash()}</button>
        </div>
      </div>
      <div id="vols-${cssId(p.name)}" style="margin-bottom:14px"></div>`;
      }).join("") || emptyState("storage pools", "Create Pool", "pool-create-empty"));
      $("#pool-create-empty")?.addEventListener("click", () => openCreatePool());
      // The whole card opens the pool's page; its buttons keep their own jobs.
      list.querySelectorAll("[data-pool-open]").forEach((card: Element) => {
        (card as HTMLElement).onclick = (e) => {
          if ((e.target as Element).closest?.("button")) return;
          selectPoolRow(selectedConn!, (card as HTMLElement).dataset.poolOpen!);
        };
      });
      list.querySelectorAll("[data-vols]").forEach((b: Element) => {
        (b as HTMLElement).onclick = () => {
          const pool = (b as HTMLElement).dataset.vols!;
          // Toggle: clicking the open pool collapses it, otherwise expand it.
          volumesOpen = volumesOpen === pool ? null : pool;
          loadVolumes(pool);
        };
      });
      list.querySelectorAll("[data-upload]").forEach((b: Element) => {
        (b as HTMLElement).onclick = () => openUpload((b as HTMLElement).dataset.upload!);
      });
      list.querySelectorAll("[data-delpool]").forEach((b: Element) => {
        (b as HTMLElement).onclick = () => deletePool((b as HTMLElement).dataset.delpool!);
      });
      // A full rebuild wiped the expanded pool's table; restore it in place.
      if (volumesOpen) void loadVolumes(volumesOpen);
      return;
    }

    // Same pools as before: update the dynamic numbers in place, leaving the
    // volume table untouched so it does not flicker every poll tick.
    for (const p of pools) {
      const card = $(`[data-pool-card="${cssId(p.name)}"]`, list);
      if (!card) continue;
      const used = p.allocation ?? 0;
      const cap = p.capacity ?? 0;
      const usedPct = cap > 0 ? Math.max(0, Math.min(100, (used / cap) * 100)) : 0;
      const fill = card.querySelector("[data-pool-fill]");
      if (fill) {
        (fill as HTMLElement).style.width = `${usedPct.toFixed(1)}%`;
        fill.classList.toggle("hot", usedPct > 85);
      }
      const pctEl = card.querySelector("[data-pool-pct]");
      if (pctEl) pctEl.textContent = `${usedPct.toFixed(usedPct >= 10 ? 0 : 1)}%`;
      const usedEl = card.querySelector("[data-pool-used]");
      if (usedEl) usedEl.textContent = `${fmtBytes(used)} used`;
      const freeEl = card.querySelector("[data-pool-free]");
      if (freeEl) freeEl.textContent = `${fmtBytes(p.available)} free`;
      const capEl = card.querySelector("[data-pool-cap]");
      if (capEl) capEl.textContent = `${fmtBytes(cap)} total`;
      const badge = card.querySelector("[data-pool-active]");
      if (badge) {
        badge.textContent = p.active ? "Active" : "Inactive";
        badge.classList.toggle("on", p.active);
      }
      card.classList.toggle("ok", p.active);
    }
  } catch (e) {
    const list = $("#pools-list");
    if (list) setHtml(list, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

async function loadVolumes(pool: string) {
  if (!selectedConn) return;
  const target = $(`#vols-${cssId(pool)}`);
  if (!target) return;
  // Collapsed: clear the table and stop.
  if (volumesOpen !== pool) {
    setHtml(target, "");
    return;
  }
  try {
    const vols = await api.get<VolumeSummary[]>(`/proxy/${selectedConn}/pools/${encodeURIComponent(pool)}/volumes`);
setHtml(target, `<table class="grid">
      <tr><th>Volume</th><th>Size</th><th>Allocated</th><th></th></tr>
      ${vols.map((v) => `
        <tr><td>${esc(v.name)}</td><td>${fmtBytes(v.capacity)}</td><td>${fmtBytes(v.allocation)}</td>
        <td><button class="btn danger icon" data-delvol="${esc(v.name)}">${ic.trash()}</button></td></tr>`).join("") || `<tr><td colspan="4" class="empty">No volumes.</td></tr>`}
    </table>
    <div class="actions"><button class="btn" id="create-vol" data-pool="${esc(pool)}">New volume</button></div>`);
    target.querySelectorAll("[data-delvol]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => deleteVolume(pool, (b as HTMLElement).dataset.delvol!);
    });
    const cv = target.querySelector("#create-vol");
    if (cv) (cv as HTMLElement).onclick = () => openCreateVolume(pool);
  } catch (e) {
setHtml(target, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

// ---------------------------------------------------------------- VM detail

/// Enable the VM header's power actions for the guest's current state.
///
/// The header is painted once per view, and on a fresh load that happens
/// before the guest list has arrived, so it painted every guest as stopped and
/// nothing corrected it: the poll refreshes the list, not the header. This
/// runs whenever the list changes, which also covers a guest that changes
/// state on its own (shut down from inside, crashed).
function syncVmActions() {
  if (!selectedConn || !selectedVm) return;
  const running = treeVms[selectedConn]?.find((x) => x.name === selectedVm)?.active === true;
  const set = (id: string, on: boolean) => {
    const b = document.getElementById(id) as HTMLButtonElement | null;
    if (b) b.disabled = !on;
  };
  set("vm-start", !running);
  for (const id of ["vm-shutdown", "vm-reboot", "vm-reset", "vm-destroy", "vm-vv"]) set(id, running);
}

async function renderVm() {
  if (!selectedConn || !selectedVm) return;
  if (isOffline(selectedConn)) return renderOffline();
  // VM view owns the pane: invalidates any in-flight resource render.
  beginRender();
  syncUrl();
  const content = $("#content");
  const vm = selectedVm;
  const title = treeVms[selectedConn]?.find((x) => x.name === vm)?.title || vm;
  const running = treeVms[selectedConn]?.find((x) => x.name === vm)?.active === true;
  const restoreTabFocus = holdFocus(content, ".tab.active");
setHtml(content, `
    <div class="dhead"><span class="dhead-ic">${ic.vm(20)}</span><span class="dtitle">${esc(title)}</span></div>
    <div class="tabbar">
      <button class="tab ${vmTab === "summary" ? "active" : ""}" data-vmtab="summary">${ic.datacenter(14)} Summary</button>
      <button class="tab ${vmTab === "console" ? "active" : ""}" data-vmtab="console">${ic.vm(14)} Console</button>
      <button class="tab ${vmTab === "hardware" ? "active" : ""}" data-vmtab="hardware">${ic.cpu(14)} Hardware</button>
      <button class="tab ${vmTab === "options" ? "active" : ""}" data-vmtab="options">${ic.options(14)} Options</button>
      <button class="tab ${vmTab === "network" ? "active" : ""}" data-vmtab="network">${ic.network(14)} Network</button>
      <button class="tab ${vmTab === "xml" ? "active" : ""}" data-vmtab="xml">${ic.xml(14)} XML</button>
      <div class="tabbar-actions row-actions">
        <button class="btn icon" id="vm-start" title="Start" ${running ? "disabled" : ""}>${ic.start()}</button>
        <button class="btn icon" id="vm-shutdown" title="Shutdown" ${running ? "" : "disabled"}>${ic.stop()}</button>
        <button class="btn icon" id="vm-reboot" title="Reboot" ${running ? "" : "disabled"}>${ic.reboot()}</button>
        <button class="btn icon" id="vm-reset" title="Reset" ${running ? "" : "disabled"}>${ic.reset()}</button>
        <button class="btn icon" id="vm-destroy" title="Force stop" ${running ? "" : "disabled"}>${ic.forceStop()}</button>
        <button class="btn icon" id="vm-vv" title="Open in virt-viewer" ${running ? "" : "disabled"}>${ic.download()}</button>
        ${canShareVm(selectedConn!, vm) ? `<button class="btn icon" id="vm-share" title="Share">${ic.perms()}</button>` : ""}
        <button class="btn danger icon" id="vm-delete" title="Delete">${ic.trash()}</button>
      </div>
    </div>
    <div id="vm-body">${vmTab === "console" ? "" : skeleton("page")}</div>`);
  restoreTabFocus();
  content.querySelectorAll("[data-vmtab]").forEach((b: Element) => {
    (b as HTMLElement).onclick = () => {
      const next = (b as HTMLElement).dataset.vmtab as typeof vmTab;
      // Leaving the console must tear down its WebSocket and observer,
      // otherwise the session keeps streaming in the background.
      if (vmTab === "console" && next !== "console") closeConsole();
      vmTab = next;
      void renderVm();
    };
  });
  // Re-read the guest states so the action buttons and sidebar reflect the
  // change immediately, rather than waiting for the 1s poll.
  const refresh = async () => {
    await refreshTreeVms();
    renderTree();
    await renderVm();
  };
  $("#vm-start").onclick = () => act(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/start`, "POST").then(refresh);
  $("#vm-shutdown").onclick = () => act(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/shutdown`, "POST").then(refresh);
  $("#vm-reset").onclick = () => act(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/reset`, "POST").then(refresh);
  $("#vm-destroy").onclick = () => act(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/destroy`, "POST").then(refresh);
  $("#vm-reboot").onclick = () => act(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/reboot`, "POST").then(refresh);
  $("#vm-delete").onclick = () => deleteVm(vm);
  const share = $("#vm-share");
  if (share) share.onclick = () => void openShareVm(selectedConn!, vm);
  const vvConn = selectedConn;
  $("#vm-vv").onclick = () => void downloadVv(vvConn, vm);
  // XML is read-only, so painting a cached answer first is safe. Summary shows
  // live statistics, which must never be painted stale, and the editing tabs
  // load once: a second paint would discard what the user typed meanwhile.
  if (vmTab === "xml") await fastLoad(() => renderVmBody(vm));
  else await renderVmBody(vm);
}

/// A network's IPAM, from the daemon (`GET /networks/{name}/ipam`).
interface IpamInfo {
  serving: boolean; gateway: string; prefix: number;
  range_start: string; range_end: string; range_size: number; range_used: number;
  dns_forwarders: string[]; domain: string | null; lease_secs: number;
}
/// One reservation (`GET /networks/{name}/hosts`).
interface IpamHost {
  mac: string; ip: string; name: string | null; dynamic: boolean;
  vm: string | null; vm_name: string | null; seen_ms: number;
}

async function renderVmBody(vm: string) {
  if (!selectedConn) return;
  const conn = selectedConn; // narrowed copy for the nested closures below
  const body = $("#vm-body");
  // The pane can be gone: the poll may fire this just as a different view
  // replaces the content (or the user navigates mid-render). Bail rather than
  // dereference null — the previous `.parentElement?.` guarded the wrong hop.
  if (!body) return;
  // Only the Hardware tab stretches to fill the pane; every other tab scrolls
  // normally. Drop the class here and let the hardware branch re-add it.
  body.parentElement?.classList.remove("vm-fill");
  if (vmTab === "console") {
    const zoomLabel = (z: number) => (z === 0 ? "Fit" : `${Math.round(z * 100)}%`);
    setHtml(body, `
      <div class="console-bar">
        <span class="console-group">
          <button class="btn icon" id="con-zoom-out" title="Zoom out">${ic.zoomOut()}</button>
          <button class="btn" id="con-zoom-reset" title="Fit to window">${zoomLabel(consoleZoom)}</button>
          <button class="btn icon" id="con-zoom-in" title="Zoom in">${ic.zoomIn()}</button>
          <button class="btn icon" id="con-fit" title="Fit to window">${ic.fit()}</button>
        </span>
        <span class="console-group" style="margin-left:auto">
          <button class="btn" id="con-cad" title="Send Ctrl+Alt+Del">Ctrl+Alt+Del</button>
        </span>
      </div>
      <div class="console-hint" id="con-hint"></div>
      <div class="console-stage" id="console-stage" tabindex="0">
        <div class="console-msg" id="console-msg">connecting…</div>
      </div>`);

    const setHint = (t: string) => {
      const h = $("#con-hint");
      if (h) h.textContent = t;
    };

    try {
      const g = await api.get<{ graphics: { kind: string } | null }>(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/graphics`);
      const kind = g.graphics?.kind ?? "vnc";
      const base = `/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/console`;

      // SPICE is not RFB and has no usable browser client, so hand the guest
      // over to virt-viewer instead of pretending to render it here.
      if (kind !== "vnc") {
        for (const id of ["#con-zoom-in", "#con-zoom-out", "#con-fit", "#con-zoom-reset", "#con-cad"]) {
          const el = $(id);
          if (el) el.disabled = true;
        }
        setHint(`${kind.toUpperCase()} cannot be displayed in a browser. Open it in virt-viewer instead.`);
        const stage = $("#console-stage");
        setHtml(stage, `
          <div class="console-offer">
            <p>This guest uses <b>${esc(kind.toUpperCase())}</b>, which needs a native client.</p>
            <button class="btn primary" id="con-vv">${ic.download(14)} Open in virt-viewer</button>
            <p class="muted">Downloads a connection file, good for one viewer within a minute. Add a VNC graphics device to use the built-in console.</p>
          </div>`);
        const vvBtn = $("#con-vv");
        if (vvBtn) vvBtn.onclick = () => void downloadVv(conn, vm);
        return;
      }

      const con = await attachConsole($("#console-stage"), kind, base, (msg) => {
        const m = $("#console-msg");
        if (m) {
          m.textContent = msg ?? "";
          m.classList.toggle("hidden", !msg);
        }
      }, (z) => {
        // A pinch changes the zoom without going near the toolbar, so the
        // label and the Ctrl+wheel stepping base have to follow it rather than
        // sit on whatever the last button click left behind.
        consoleZoom = z;
        const b = $("#con-zoom-reset");
        if (b) b.textContent = zoomLabel(z);
      });

      if (!con) {
        // The daemon refused to attach. Leave the toolbar visible but inert
        // rather than silently dead, and say why.
        for (const id of ["#con-zoom-in", "#con-zoom-out", "#con-fit", "#con-zoom-reset", "#con-cad"]) {
          const el = $(id);
          if (el) el.disabled = true;
        }
        setHint("Console unavailable — controls disabled until it connects.");
        return;
      }

      setHint(isCoarsePointer()
        ? "Tap to click, drag to drag, two-finger to scroll, pinch to zoom."
        : "Click the screen to grab input. Ctrl+wheel zooms.");

      const setZoomLabel = () => {
        const b = $("#con-zoom-reset");
        if (b) b.textContent = zoomLabel(con.getZoom());
      };
      const nudge = (dir: 1 | -1) => {
        // Stepping from Fit starts at 1:1 so the first click is predictable.
        const base = con.getZoom() || 1;
        const next = Math.min(4, Math.max(0.25, +(base + dir * 0.25).toFixed(2)));
        con.setZoom(next);
        consoleZoom = next;
        setZoomLabel();
      };
      $("#con-zoom-in").onclick = () => nudge(1);
      $("#con-zoom-out").onclick = () => nudge(-1);
      const fit = () => { con.setZoom(0); consoleZoom = 0; setZoomLabel(); };
      $("#con-fit").onclick = fit;
      $("#con-zoom-reset").onclick = fit;

      // Ctrl+wheel zooms locally instead of scrolling the guest.
      $("#console-stage").addEventListener("wheel", (e: WheelEvent) => {
        if (!e.ctrlKey) return;
        e.preventDefault();
        nudge(e.deltaY < 0 ? 1 : -1);
      }, { passive: false });

      $("#con-cad").onclick = () => void con.sendCtrlAltDel();

      // Re-fit when the pane resizes, so the guest stays centred.
      const stage = $("#console-stage");
      const ro = new ResizeObserver(() => con.refit());
      ro.observe(stage);
      consoleResizeObserver?.disconnect();
      consoleResizeObserver = ro;

      setZoomLabel();
    } catch (e) {
      const m = $("#console-msg");
      if (m) m.textContent = (e as Error).message;
    }
    return;
  }
  if (vmTab === "options") {
    try {
      const [{ xml }, info] = await Promise.all([
        api.get<{ xml: string }>(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/xml`),
        api.get<any>(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}`),
      ]);
      const pick = (re: RegExp, fallback = "—") => xml.match(re)?.[1] ?? fallback;
      const title = pick(/<title>([^<]*)<\/title>/, "");
      const description = pick(/<description>([^<]*)<\/description>/, "");
      const bootDev = pick(/<boot dev='([^']*)'/, "hd");
      const machine = pick(/machine='([^']*)'/);
      const arch = pick(/arch='([^']*)'/);
      const onPoweroff = pick(/<on_poweroff>([^<]*)<\/on_poweroff>/);
      const onReboot = pick(/<on_reboot>([^<]*)<\/on_reboot>/);
      const onCrash = pick(/<on_crash>([^<]*)<\/on_crash>/);
      const autostartHint = info.persistent ? "persistent" : "transient";
      const running = info.active === true;

      setHtml(body, `
        <div class="actions">
          <button class="btn primary" id="opt-save">${ic.edit(14)} Save Options</button>
          ${running ? `<span class="hw-locked">Changes apply after the guest is restarted.</span>` : ""}
        </div>
        <div class="hwtable">
          <div class="hwrow">
            <span class="hwrow-ic">${ic.vm()}</span>
            <div class="hwrow-label"><b>Name</b><i>Renamed via the dedicated endpoint</i></div>
            <div class="hwctl"><input id="opt-name" value="${esc(vm)}"></div>
          </div>
          <div class="hwrow">
            <span class="hwrow-ic">${ic.edit()}</span>
            <div class="hwrow-label"><b>Title</b></div>
            <div class="hwctl"><input id="opt-title" value="${esc(title)}" placeholder="short label"></div>
          </div>
          <div class="hwrow">
            <span class="hwrow-ic">${ic.options()}</span>
            <div class="hwrow-label"><b>Description</b></div>
            <div class="hwctl"><textarea id="opt-desc" rows="2" placeholder="free-form notes">${esc(description)}</textarea></div>
          </div>
          <div class="hwrow">
            <span class="hwrow-ic">${ic.disk()}</span>
            <div class="hwrow-label"><b>Boot Device</b></div>
            <div class="hwctl"><select id="opt-boot">
              ${["hd", "cdrom", "network"].map((d) =>
                `<option value="${d}" ${d === bootDev ? "selected" : ""}>${d}</option>`).join("")}
            </select></div>
          </div>
          <div class="hwrow">
            <span class="hwrow-ic">${ic.key()}</span>
            <div class="hwrow-label"><b>UUID</b></div>
            <div class="hwctl hwctl-static">${esc(info.uuid ?? "—")}</div>
          </div>
        </div>
        <div class="section-title" style="margin-top:18px">Lifecycle</div>
        <div class="hwtable">
          <div class="hwrow">
            <span class="hwrow-ic">${ic.stop()}</span>
            <div class="hwrow-label"><b>On Poweroff</b></div>
            <div class="hwctl hwctl-static">${esc(capLabel(onPoweroff))}</div>
          </div>
          <div class="hwrow">
            <span class="hwrow-ic">${ic.reboot()}</span>
            <div class="hwrow-label"><b>On Reboot</b></div>
            <div class="hwctl hwctl-static">${esc(capLabel(onReboot))}</div>
          </div>
          <div class="hwrow">
            <span class="hwrow-ic">${ic.forceStop()}</span>
            <div class="hwrow-label"><b>On Crash</b></div>
            <div class="hwctl hwctl-static">${esc(capLabel(onCrash))}</div>
          </div>
          <div class="hwrow">
            <span class="hwrow-ic">${ic.autostart()}</span>
            <div class="hwrow-label"><b>Persistence</b></div>
            <div class="hwctl hwctl-static">${esc(capLabel(autostartHint))}</div>
          </div>
          <div class="hwrow">
            <span class="hwrow-ic">${ic.cpu()}</span>
            <div class="hwrow-label"><b>Machine</b></div>
            <div class="hwctl hwctl-static">${esc(machine)}</div>
          </div>
          <div class="hwrow">
            <span class="hwrow-ic">${ic.server()}</span>
            <div class="hwrow-label"><b>Architecture</b></div>
            <div class="hwctl hwctl-static">${esc(arch)}</div>
          </div>
        </div>`);

      $("#opt-save")?.addEventListener("click", async () => {
        const newTitle = $("#opt-title").value.trim();
        const newDesc = $("#opt-desc").value.trim();
        const newBoot = ($("#opt-boot") as HTMLSelectElement).value;
        const newName = $("#opt-name").value.trim();

        // Rename first: the rename endpoint is the only safe way to swap the
        // name (it keeps the UUID), and the XML write below must both target
        // the new name and carry a matching <name> or it would undo the swap.
        let target = vm;
        if (newName && newName !== vm) {
          try {
            await api.post(
              `/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/rename`,
              { name: newName });
            target = newName;
            if (selectedVm === vm) selectedVm = newName;
            await refreshTreeVms();
            renderTree();
            void renderVm();
          } catch (e) {
            toast((e as Error).message, "bad");
            return;
          }
        }

        // Rewrite only the elements this form owns, leaving the rest of the
        // definition untouched.
        let next = xml
          .replace(/\s*<title>[^<]*<\/title>/, "")
          .replace(/\s*<description>[^<]*<\/description>/, "")
          .replace(/<boot dev='[^']*'\/>/, `<boot dev='${esc(newBoot)}'/>`)
          .replace(/<name>[^<]*<\/name>/, `<name>${esc(target)}</name>`);
        const meta = [
          newTitle ? `  <title>${esc(newTitle)}</title>` : "",
          newDesc ? `  <description>${esc(newDesc)}</description>` : "",
        ].filter(Boolean).join("\n");
        if (meta) {
          // libvirt requires title/description right after <name>.
          next = next.replace(/(<name>[^<]*<\/name>)/, `$1\n${meta}`);
        }
        void runTask(`Update options for ${target}`, async () => {
          await api.putXml(`/proxy/${selectedConn}/domains/${encodeURIComponent(target)}/xml`, next);
        }, { done: `Options for ${target} saved`, notify: toast });
      });
    } catch (e) {
      setHtml(body, `<div class="empty">${esc((e as Error).message)}</div>`);
    }
    return;
  }

  if (vmTab === "network") {
    // Everything about this guest's networking on one page: its interfaces
    // (hot-added and removed, live), their DHCP reservations and rate limits,
    // and its firewall. Interfaces are read from the live guest each render,
    // so a NIC added from the Hardware tab or Create VM simply appears here.
    const encVm = encodeURIComponent(vm);
    type Nic = {
      dev: string | null; mac: string | null; model: string | null; network: string | null;
      rx_mbps: number | null; tx_mbps: number | null;
    };
    let nics: Nic[] = [];
    let nets: NetworkSummary[] = [];
    let running = false;
    // Network name -> its IPAM state (subnet, range) and reservations, from
    // the daemon's own IPAM. Absent for a network it does not serve.
    const netIpam = new Map<string, { ipam: IpamInfo; hosts: IpamHost[] }>();
    try {
      const [ifs, ns, info] = await Promise.all([
        api.get<{ interfaces: Nic[] }>(`/proxy/${conn}/domains/${encVm}/interfaces`),
        api.get<NetworkSummary[]>(`/proxy/${conn}/networks`),
        api.get<{ active: boolean }>(`/proxy/${conn}/domains/${encVm}`),
      ]);
      nics = ifs.interfaces ?? [];
      nets = ns;
      running = info.active === true;
      const used = [...new Set(nics.map((n) => n.network).filter(Boolean) as string[])];
      await Promise.all(used.map(async (name) => {
        const base = `/proxy/${conn}/networks/${encodeURIComponent(name)}`;
        try {
          const [ipam, h] = await Promise.all([
            api.get<IpamInfo>(`${base}/ipam`),
            api.get<{ hosts: IpamHost[] }>(`${base}/hosts`),
          ]);
          netIpam.set(name, { ipam, hosts: h.hosts });
          return null;
        } catch {
          // Not served here (no IPv4 subnet): no IPAM for it. Both this and the
          // return in the try are for array-callback-return, which wants a
          // return on each arm of the try/catch; nothing consumes the value,
          // so do not "tidy" one of them away.
          return null;
        }
      }));
    } catch (e) {
      setHtml(body, `<div class="empty">${esc((e as Error).message)}</div>`);
      return;
    }
    if (!stillCurrentVm(conn, vm, "network")) return;

    const reservation = (n: Nic) =>
      (n.network ? netIpam.get(n.network)?.hosts : undefined)
        ?.find((h) => h.mac === (n.mac ?? "").toLowerCase());
    const fmtRateCell = (v: number | null) => (v ? `${v} Mbps` : "Unlimited");
    const liveNote = running
      ? "Changes apply to the running guest immediately and are kept for its next boot."
      : "The guest is shut off: changes are saved to its definition.";

    setHtml(body, `
      <div class="section-title fw-cat">Interfaces</div>
      <div class="dsub" style="margin-bottom:8px">${liveNote}</div>
      <table class="grid" id="vmnet-nics">
        <tr><th>Device</th><th>MAC</th><th>Network</th><th>Model</th><th>IPAM address</th><th>RX limit</th><th>TX limit</th><th class="col-actions"></th></tr>
        ${nics.map((n, i) => {
          const r = reservation(n);
          const hasDhcp = !!(n.network && netIpam.has(n.network));
          return `<tr data-nic="${i}">
            <td>${esc(n.dev ?? "—")}</td>
            <td><code>${esc(n.mac ?? "—")}</code></td>
            <td>${esc(n.network ?? "—")}</td>
            <td>${esc(n.model ?? "—")}</td>
            <td>${r
              ? `${esc(r.ip)}${r.dynamic ? ` <span class="state" title="Assigned from the range when the guest first asked; kept until the VM is deleted">auto</span>` : ""}`
              : `<span class="state">${hasDhcp ? "Not yet assigned" : "No DHCP"}</span>`}</td>
            <td>${fmtRateCell(n.rx_mbps)}</td>
            <td>${fmtRateCell(n.tx_mbps)}</td>
            <td class="col-actions">
              <button class="btn icon" data-nic-ipam="${i}" title="IPAM address" ${hasDhcp ? "" : "disabled"}>${ic.edit()}</button>
              <button class="btn icon" data-nic-rate="${i}" title="Rate limit">${ic.autostart()}</button>
              <button class="btn danger icon" data-nic-del="${i}" title="Remove interface">${ic.trash()}</button>
            </td></tr>`;
        }).join("") || `<tr><td colspan="8" class="empty">No network interfaces.</td></tr>`}
      </table>
      <div class="actions"><button class="btn" id="vmnet-add">${ic.plus(14)} Add interface</button></div>
      <div class="section-title fw-cat" style="margin-top:22px">Firewall</div>
      <div id="vmnet-fw">${skeleton("table")}</div>`);

    const reload = () => { if (stillCurrentVm(conn, vm, "network")) void renderVmBody(vm); };

    $("#vmnet-add", body).onclick = () => {
      const el = openModal("Add interface", `
        <label class="field"><span>Network</span><select id="nic-net">
          ${nets.map((n) => `<option value="${esc(n.name)}">${esc(n.name)}${n.active ? "" : " (inactive)"}</option>`).join("")}
        </select></label>
        <label class="field"><span>Model</span><select id="nic-model">
          <option value="virtio">virtio</option><option value="e1000e">e1000e</option><option value="rtl8139">rtl8139</option>
        </select></label>
        <label class="field"><span>MAC address (optional)</span><input id="nic-mac" placeholder="generated if blank"></label>
        <div class="dsub">${running ? "Hot-added to the running guest." : "Added to the guest's definition."}</div>`,
        `<button class="btn primary" id="nic-ok">${ic.plus(14)} Add</button>`);
      $("#nic-ok", el).onclick = async () => {
        try {
          const r = await api.post<{ mac: string; warning?: string }>(`/proxy/${conn}/domains/${encVm}/interfaces`, {
            network: ($("#nic-net", el) as HTMLSelectElement).value,
            model: ($("#nic-model", el) as HTMLSelectElement).value,
            mac: ($("#nic-mac", el) as HTMLInputElement).value.trim() || undefined,
          });
          el.remove();
          toast(r.warning ?? `Interface ${r.mac} added`, r.warning ? "bad" : "ok");
          reload();
        } catch (e) { toast((e as Error).message, "bad"); }
      };
    };

    body.querySelectorAll("[data-nic-del]").forEach((b: Element) => {
      (b as HTMLElement).onclick = async () => {
        const n = nics[Number((b as HTMLElement).dataset.nicDel)];
        if (!n?.mac) return;
        const ok = await askConfirm("Remove interface",
          `Remove ${n.mac} (${n.network ?? "no network"})${running ? " from the running guest" : ""}?`, "Remove", true);
        if (!ok) return;
        try {
          const r = await api.del<{ pending: boolean; warning?: string }>(
            `/proxy/${conn}/domains/${encVm}/interfaces/${encodeURIComponent(n.mac)}`);
          // An unplug is a request to the guest OS; say so when it is still
          // holding the device, instead of claiming it is gone.
          toast(r.pending
            ? "Removed from the definition; the running guest has not released it yet"
            : r.warning ?? "Interface removed", r.pending || r.warning ? "bad" : "ok");
          reload();
        } catch (e) { toast((e as Error).message, "bad"); }
      };
    });

    body.querySelectorAll("[data-nic-rate]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => {
        const n = nics[Number((b as HTMLElement).dataset.nicRate)];
        if (!n?.mac) return;
        const el = openModal(`Rate limit — ${n.mac}`, `
          <label class="field"><span>RX limit, Mbps (to the guest; blank = unlimited)</span>
            <input id="rate-rx" type="number" min="0" step="any" value="${n.rx_mbps ?? ""}"></label>
          <label class="field"><span>TX limit, Mbps (from the guest; blank = unlimited)</span>
            <input id="rate-tx" type="number" min="0" step="any" value="${n.tx_mbps ?? ""}"></label>
          <div class="dsub">${running ? "Applied to the running guest immediately." : "Applied when the guest next starts."}</div>`,
          `<button class="btn primary" id="rate-ok">Apply</button>`);
        const num = (id: string) => {
          const v = ($(id, el) as HTMLInputElement).value.trim();
          return v === "" ? undefined : Number(v);
        };
        $("#rate-ok", el).onclick = async () => {
          try {
            await api.put(`/proxy/${conn}/domains/${encVm}/interfaces/${encodeURIComponent(n.mac!)}/bandwidth`,
              { rx_mbps: num("#rate-rx"), tx_mbps: num("#rate-tx") });
            el.remove();
            toast("Rate limit applied");
            reload();
          } catch (e) { toast((e as Error).message, "bad"); }
        };
      };
    });

    body.querySelectorAll("[data-nic-ipam]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => {
        const n = nics[Number((b as HTMLElement).dataset.nicIpam)];
        const ipamNet = n?.network ? netIpam.get(n.network) : undefined;
        const mac = n?.mac;
        if (!mac || !n.network || !ipamNet) return;
        const { ipam } = ipamNet;
        const current = reservation(n);
        const suggested = current?.ip
          ?? nextFreeIp(`${ipam.gateway}/${ipam.prefix}`, ipamNet.hosts.map((h) => h.ip),
            { start: ipam.range_start, end: ipam.range_end }) ?? "";
        const el = openModal(`IPAM address — ${n.mac}`, `
          <label class="field"><span>IP address on ${esc(n.network)} (${esc(ipam.gateway)}/${ipam.prefix})</span>
            <input id="ipam-ip" value="${esc(suggested)}" placeholder="blank = assign automatically"></label>
          <label class="field"><span>Hostname (optional)</span>
            <input id="ipam-name" value="${esc(current?.name ?? vm)}"></label>
          <div class="dsub">The guest gets this address at its next DHCP exchange; nothing is restarted.
            Every address is kept until the VM is deleted.</div>`,
          `${current ? `<button class="btn danger" id="ipam-clear">Release</button>` : ""}
           <button class="btn primary" id="ipam-ok">Save</button>`);
        const hostUrl = `/proxy/${conn}/networks/${encodeURIComponent(n.network)}/hosts`;
        $("#ipam-ok", el).onclick = async () => {
          const ip = ($("#ipam-ip", el) as HTMLInputElement).value.trim();
          try {
            if (!ip) await api.del(`${hostUrl}/${encodeURIComponent(mac)}`);
            else await api.put(hostUrl, { mac, ip, name: ($("#ipam-name", el) as HTMLInputElement).value.trim() || undefined });
            el.remove();
            toast(ip ? `Reserved ${ip}` : "Released: the guest gets an address from the range at its next DHCP");
            reload();
          } catch (e) { toast((e as Error).message, "bad"); }
        };
        const clear = $("#ipam-clear", el);
        if (clear) clear.onclick = async () => {
          try {
            await api.del(`${hostUrl}/${encodeURIComponent(mac)}`);
            el.remove();
            toast("Released: the guest gets an address from the range at its next DHCP");
            reload();
          } catch (e) { toast((e as Error).message, "bad"); }
        };
      };
    });

    // Firewall: rules scoped to this guest by its interface MACs.
    const rulesBody = $("#vmnet-fw", body) as HTMLElement;
    let vmRules: FirewallRule[] = [];
    let vmPolicy: FirewallPolicy = blankPolicy();
    try {
      const fw = await api.get<{ rules: FirewallRule[]; policy: FirewallPolicy }>(
        `/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/firewall`);
      vmRules = fw.rules ?? [];
      vmPolicy = fw.policy ?? blankPolicy();
    } catch {
      vmRules = [];
    }

    const readRules = () => {
      vmRules = readFwRuleRows(rulesBody);
      vmPolicy = readFwPolicy(rulesBody);
    };
    // Addresses to offer in this guest's source/dest fields.
    let ipCandidates: string[] = [];
    void collectIpCandidates(conn).then((ips) => { ipCandidates = ips; });
    const renderRules = () => {
      setHtml(rulesBody, `
        <div class="section-title fw-cat">Default Policies</div>
        ${fwPolicyRow(vmPolicy)}
        <div class="section-title fw-cat">Rules</div>
        <table class="grid">
          <tr><th>Direction</th><th>Action</th><th>Proto</th><th>Source IP</th><th>Dest IP</th><th>Source Port</th><th>Dest Port</th><th></th></tr>
          ${vmRules.map(fwRuleRow).join("") || `<tr><td colspan="8" class="empty">No rules — traffic is accepted.</td></tr>`}
        </table>
        <div class="actions">
          <button class="btn" id="vmfw-add">${ic.plus(14)} Add rule</button>
          <button class="btn primary" id="vmfw-apply">${ic.firewall(14)} Apply</button>
          <button class="btn al-open" id="vmfw-logs">${ic.logs(14)} Access logs</button>
        </div>`);
      $("#vmfw-logs", rulesBody).onclick = () => openAccessLogs(conn, vm);
      attachRuleIpSuggest(rulesBody, () => ipCandidates);
      $("#vmfw-add", rulesBody).onclick = () => {
        readRules();
        vmRules.push(blankRule());
        renderRules();
      };
      $("#vmfw-apply", rulesBody).onclick = async () => {
        readRules();
        try {
          await api.put(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/firewall`, { rules: vmRules, policy: vmPolicy });
          toast("Firewall applied");
        } catch (e) {
          toast((e as Error).message, "bad");
        }
      };
      rulesBody.querySelectorAll("[data-r-del]").forEach((b: Element) => {
        (b as HTMLElement).onclick = () => {
          readRules();
          const tr = (b as HTMLElement).closest("tr")!;
          // Same header-offset bug as the network editor: count data rows.
          const rows = [...rulesBody.querySelectorAll("[data-r-dir]")].map((s) => (s as HTMLElement).closest("tr")!);
          vmRules.splice(rows.indexOf(tr), 1);
          renderRules();
        };
      });
    };
    renderRules();
    return;
  }

  if (vmTab === "hardware") {
    // The hardware editor is embedded here — no read-only table and no
    // "Edit hardware" click. Devices are edited in place and saved.
    await openHardwareEditor({ vm, inline: true });
    return;
  }
  if (vmTab === "xml") {
    try {
      const { xml } = await api.get<{ xml: string }>(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/xml`);
setHtml(body, `<div class="cmd">${esc(xml)}</div>`);
    } catch (e) {
setHtml(body, `<div class="empty">${esc((e as Error).message)}</div>`);
    }
    return;
  }
  // summary: six cards — CPU, Memory, Disks, Network, Firewall, Hardware.
  try {
    const [d, s, xr, fw, nic] = await Promise.all([
      api.get<any>(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}`),
      api.get<DomainStats>(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/stats`),
      api.get<{ xml: string }>(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/xml`),
      api.get<{ rules: FirewallRule[]; policy: FirewallPolicy }>(
        `/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/firewall`)
        .catch(() => ({ rules: [] as FirewallRule[], policy: blankPolicy() })),
      api.get<{ interfaces: { dev: string | null; mac: string | null; model: string | null; rx_bytes: number; tx_bytes: number }[] }>(
        `/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/interfaces`)
        .catch(() => ({ interfaces: [] })),
    ]);
    const xml = xr.xml;

    const live1 = addSample(s);
    const { cpuNow, memNow } = live1;
    let cpu: (number | null)[] = live1.cpu;
    let mem: (number | null)[] = live1.mem;
    let timing: Timing | undefined;
    const running = s.state === "running";
    const usedKib = s.usable_kib != null
      ? Math.max(0, s.memory_kib - s.usable_kib)
      : (s.rss_kib ?? s.memory_kib);

    // Disks and NICs come straight from the domain XML so file-backed and
    // volume-backed devices both show, not just the ones the editor models.
    const xattr = (tag: string, name: string) =>
      tag.match(new RegExp(`${name}\\s*=\\s*['"]([^'"]*)['"]`))?.[1] ?? "";
    const disks = [...xml.matchAll(/<disk\b[^>]*device=['"](disk|cdrom)['"][\s\S]*?<\/disk>/g)].map((m) => {
      const block = m[0];
      const src = block.match(/<source\b[^>]*>/)?.[0] ?? "";
      const dev = xattr(block.match(/<target\b[^>]*>/)?.[0] ?? "", "dev");
      const kind = m[1] === "cdrom" ? "cdrom" : "disk";
      const volume = xattr(src, "volume");
      return {
        dev: dev || (kind === "cdrom" ? "cdrom" : "disk"),
        kind,
        pool: xattr(src, "pool"),
        volume,
        file: xattr(src, "file"),
        label: volume || xattr(src, "file") || (kind === "cdrom" ? "empty" : "—"),
      };
    }).filter((x) => x.kind !== "cdrom" || !x.volume?.toLowerCase().endsWith(".iso"));
    const nics = [...xml.matchAll(/<interface\b[^>]*type=['"]network['"][\s\S]*?<\/interface>/g)].map((m) => {
      const block = m[0];
      return {
        net: xattr(block.match(/<source\b[^>]*>/)?.[0] ?? "", "network"),
        model: xattr(block.match(/<model\b[^>]*>/)?.[0] ?? "", "type"),
      };
    });
    const devices = devicesFromXml(xml);
    const bios = /pflash|OVMF/i.test(xml) ? "UEFI (OVMF)" : "BIOS (SeaBIOS)";
    const display = /<graphics[^>]*type=['"]spice/.test(xml) ? "spice" : "vnc";

    // Allocation/usage for volume-backed disks comes from the pool's volume
    // table; file-backed disks have no volume record, so they show path only.
    const volUsage = new Map<string, { capacity: number; allocation: number }>();
    const backingPools = new Set(disks.map((x) => x.pool).filter(Boolean) as string[]);
    for (const pool of backingPools) {
      try {
        const vols = await api.get<VolumeSummary[]>(
          `/proxy/${selectedConn}/pools/${encodeURIComponent(pool)}/volumes`);
        for (const v of vols) volUsage.set(`${pool}/${v.name}`, { capacity: v.capacity ?? 0, allocation: v.allocation ?? 0 });
      } catch { /* a missing pool just leaves its disks without usage data */ }
    }
    // The selected disk must stay selected across the 1s poll; if it vanished
    // (disk removed), fall back to the first disk.
    const diskSel = summaryDiskSel[vm] && disks.some((x) => x.dev === summaryDiskSel[vm])
      ? summaryDiskSel[vm]
      : disks[0]?.dev;
    const selDisk = disks.find((x) => x.dev === diskSel);
    const selUsage = selDisk?.pool && selDisk?.volume
      ? volUsage.get(`${selDisk.pool}/${selDisk.volume}`)
      : undefined;
    // A cdrom has no allocation meaning; a file disk has no usage record.
    const selAlloc = selUsage?.allocation ?? 0;
    const selCap = selUsage?.capacity ?? 0;
    const selPct = selCap > 0 ? Math.max(0, Math.min(100, (selAlloc / selCap) * 100)) : 0;
    const selDiskKey = `${vm}:${selDisk?.dev ?? "disk"}`;
    let diskHist: (number | null)[] = addDiskSample(selDiskKey, selPct);
    let diskTiming: Timing | undefined;

    // The NIC list: live counters from the daemon when present, otherwise
    // the XML inventory (a shut-off guest has no counters, but still shows).
    const nicList = (nic.interfaces ?? []).map((x, i) => ({
      key: x.dev || x.mac || `nic${i}`,
      dev: x.dev || x.mac || `nic${i}`,
      model: x.model || nics[i]?.model || "virtio",
      rx: x.rx_bytes ?? 0,
      tx: x.tx_bytes ?? 0,
    }));
    const netSel = summaryNetSel[vm] && nicList.some((x) => x.key === summaryNetSel[vm])
      ? summaryNetSel[vm]
      : nicList[0]?.key;
    const selNic = nicList.find((x) => x.key === netSel);
    const netKey = `${vm}:${selNic?.key ?? "net"}`;
    const liveNet = addNetSample(netKey, selNic?.rx ?? 0, selNic?.tx ?? 0, s.sampled_at_ms);
    const { rxNow, txNow } = liveNet;
    let rx: (number | null)[] = liveNet.rx;
    let tx: (number | null)[] = liveNet.tx;
    let netTiming: Timing | undefined;
    if (!isLive()) {
      const shown = range().key;
      const nicMac = (nic.interfaces ?? []).find((x, i) => (x.dev || x.mac || `nic${i}`) === selNic?.key)?.mac;
      const q = { conn, name: vm };
      const none = Promise.resolve(null);
      try {
        const [hv, hd, hn] = await Promise.all([
          loadHistory(api.get, { ...q, kind: "vm" }),
          selDisk ? loadHistory(api.get, { ...q, kind: "disk", sub: selDisk.dev }) : none,
          nicMac ? loadHistory(api.get, { ...q, kind: "nic", sub: nicMac }) : none,
        ]);
        ({ a: cpu, b: mem, timing } = hv);
        // Braced, not `if (hd) ({ ... } = hd)`: the unbraced form is a
        // destructuring assignment that reads as a mistyped comparison.
        if (hd) { ({ a: diskHist, timing: diskTiming } = hd); }
        if (hn) { ({ a: rx, b: tx, timing: netTiming } = hn); }
      } catch { /* keep the live minute */ }
      if (range().key !== shown || !stillCurrentVm(conn, vm, "summary")) return;
    }
    const netMax = Math.max(1, ...rx.map((v) => v ?? 0), ...tx.map((v) => v ?? 0));

    // Every hardware device in one full-width strip, in domain order.
    const hwStrip = [
      { icon: ic.cpu(), label: "BIOS", sum: bios },
      { icon: ic.vm(), label: "Display", sum: display },
      ...devices.map((x) => ({
        icon: ic[hwType(x.kind)?.icon ?? "cpu"](),
        label: hwType(x.kind)?.label ?? x.kind,
        sum: deviceSummary(x),
      })),
    ];

    setHtml(body, `
    ${rangeBar()}
    <div class="sumgrid vm-sumgrid vm-top">
      <div class="metric ${running ? "ok" : ""}">
        <div class="metric-head">
          <span class="metric-ic">${ic.cpu()}</span>
          <b>CPU</b>
          <span class="metric-value">${pct(cpuNow)}</span>
        </div>
        <div class="meter"><div class="meter-fill ${cpuNow > 85 ? "hot" : ""}" style="width:${cpuNow.toFixed(1)}%"></div></div>
        <div class="chart">
          ${sparkline(cpu, "", 100, timing)}
          <div class="chart-axis"><span>${range().axis}</span><span>now</span></div>
        </div>
        <div class="metric-foot"><span>${s.vcpus} vCPU</span><span>${esc(capLabel(d.state))}</span></div>
      </div>

      <div class="metric ${running ? "ok" : ""}">
        <div class="metric-head">
          <span class="metric-ic">${ic.memory()}</span>
          <b>Memory</b>
          <span class="metric-value">${pct(memNow)}</span>
        </div>
        <div class="meter"><div class="meter-fill ${memNow > 85 ? "hot" : ""}" style="width:${memNow.toFixed(1)}%"></div></div>
        <div class="chart">
          ${sparkline(mem, "mem", 100, timing)}
          <div class="chart-axis"><span>${range().axis}</span><span>now</span></div>
        </div>
        <div class="metric-foot">
          <span>${fmtBytes(usedKib * 1024)} used</span>
          <span>${fmtBytes(s.max_memory_kib * 1024)} max</span>
        </div>
      </div>

      <div class="metric ${running ? "ok" : ""}">
        <div class="metric-head">
          <span class="metric-ic">${ic.disk()}</span>
          <b>Disks</b>
          ${disks.length > 1 ? `<label class="field metric-sel"><select id="disk-sel">${disks.map((x) =>
            `<option value="${esc(x.dev)}" ${x.dev === diskSel ? "selected" : ""}>${esc(x.dev)} — ${esc(x.label)}</option>`).join("")}
          </select></label>` : ""}
          <span class="metric-value">${selCap > 0 ? pct(selPct) : disks.length}</span>
        </div>
        <div class="meter"><div class="meter-fill ${selPct > 85 ? "hot" : ""}" style="width:${selPct.toFixed(1)}%"></div></div>
        <div class="chart">
          ${sparkline(diskHist, "disk", 100, diskTiming)}
          <div class="chart-axis"><span>${range().axis}</span><span>now</span></div>
        </div>
        <div class="metric-foot">
          <span>${selCap > 0 ? `${fmtBytes(selAlloc)} used` : "usage unknown"}</span>
          <span>${selCap > 0 ? fmtBytes(selCap) : esc(selDisk?.label ?? "—")}</span>
        </div>
      </div>
    </div>

    <div class="sumgrid vm-sumgrid vm-mid">
      <div class="metric ${running ? "ok" : ""}">
        <div class="metric-head">
          <span class="metric-ic">${ic.network()}</span>
          <b>Network</b>
          ${nicList.length > 1 ? `<label class="field metric-sel"><select id="net-sel">${nicList.map((x) =>
            `<option value="${esc(x.key)}" ${x.key === netSel ? "selected" : ""}>${esc(x.dev)}</option>`).join("")}
          </select></label>` : ""}
          <span class="metric-value">${fmtRate(Math.max(rxNow, txNow))}</span>
        </div>
        <div class="chart net-chart">
          <div class="net-legend"><span class="rx">▼ RX ${fmtRate(rxNow)}</span><span class="tx">▲ TX ${fmtRate(txNow)}</span></div>
          ${sparkline(rx, "rx", netMax, netTiming)}
          ${sparkline(tx, "tx", netMax, netTiming)}
          <div class="chart-axis"><span>${range().axis}</span><span>now</span></div>
        </div>
        <div class="metric-foot">
          <span>${selNic?.dev ?? "—"}</span>
          <span>${esc(selNic?.model ?? "virtio")}</span>
        </div>
      </div>

      <div class="metric">
        <div class="metric-head">
          <span class="metric-ic">${ic.firewall()}</span>
          <b>Firewall</b>
          <span class="metric-value">${fw.rules.length} rules</span>
        </div>
        <div class="fw-policy-grid">
          ${[
            ["In", fw.policy.in, ic.down()],
            ["Out", fw.policy.out, ic.up()],
            ["Forward", fw.policy.forward, ic.bidir()],
          ].map(([k, v, ico]) => {
            const action = v as FirewallPolicy["in"];
            const cls = action === "accept" ? "ok" : action === "reject" ? "warn" : "bad";
            return `<div class="fw-policy-chip ${cls}"><div class="fw-policy-val"><span>${ico}</span><b>${esc(k)}</b></div><i>${esc(capLabel(action))}</i></div>`;
          }).join("")}
        </div>
        <div class="metric-foot">
          <span>${fw.rules.length ? `${fw.rules.length} active` : "no rules"}</span>
          <span>default policies</span>
        </div>
      </div>
    </div>

    <div class="section-title fw-cat">Hardware</div>
    <div class="hw-strip">
      ${hwStrip.map((h) => `
        <div class="hw-chip">
          <span class="hw-chip-ic">${h.icon}</span>
          <span class="hw-chip-tx"><b>${esc(h.label)}</b><i>${esc(h.sum)}</i></span>
        </div>`).join("")}
    </div>`);

    $("#disk-sel", body)?.addEventListener("change", (e: Event) => {
      summaryDiskSel[vm] = (e.target as HTMLSelectElement).value;
    });
    $("#net-sel", body)?.addEventListener("change", (e: Event) => {
      summaryNetSel[vm] = (e.target as HTMLSelectElement).value;
    });
  } catch (e) {
    setHtml(body, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

// ---------------------------------------------------------------- actions

async function act(url: string, method: "POST" | "DELETE" = "POST") {
  try {
    const res = method === "POST" ? await api.post(url) : await api.del(url);
    toast("OK");
    return res;
  } catch (e) {
    toast((e as Error).message, "bad");
    throw e;
  }
}

/// Result of `DELETE /domains/{name}`: which backing stores went, which were
/// deliberately kept (with a reason), and which could not be removed.
interface DomainDeleteResult {
  removed?: string[];
  kept?: { disk: string; reason: string }[];
  failed?: { disk: string; error: string }[];
}

/// Plain-language reason for a disk the daemon kept, so the user is never left
/// wondering why a "delete the disks" request left something behind.
function keepReasonText(reason: string): string {
  if (reason === "iso") return "ISO";
  if (reason === "readonly") return "read-only";
  if (reason === "in_use") return "in use by another guest";
  return reason;
}

async function deleteVm(name: string) {
  if (!selectedConn) return;
  if (!(await askConfirm("Delete VM", `Delete VM "${name}"?`, "Delete", true))) return;
  const removeStorage = await askConfirm(
    "Remove storage",
    // Say what will NOT go: an ISO the guest boots from is shared media, and a
    // disk another guest uses is not this VM's to delete.
    `Also delete the disks belonging to "${name}"? ISOs and disks other guests use are kept.`,
    "Delete disks",
    true,
  );
  try {
    const res = await api.del<DomainDeleteResult>(
      `/proxy/${selectedConn}/domains/${encodeURIComponent(name)}?remove_storage=${removeStorage}`);
    selectedVm = null;
    const removed = res?.removed ?? [];
    const kept = res?.kept ?? [];
    const failed = res?.failed ?? [];

    // Report the whole outcome, not just "Deleted": a partial cleanup that
    // looks complete is how a stale disk gets forgotten about.
    const lines: string[] = [];
    if (removed.length) lines.push(`Deleted ${removed.length} disk(s)`);
    if (kept.length) {
      lines.push(`Kept ${kept.map((k) => `${k.disk} (${keepReasonText(k.reason)})`).join(", ")}`);
    }
    if (failed.length) lines.push(`Could not delete ${failed.map((f) => f.disk).join(", ")}`);
    toast(lines.length ? lines.join("\n") : "Deleted", failed.length ? "bad" : "ok");

    renderConn();
    // The kept disks are still on the host, so keep the sidebar counts honest.
    void refreshTreeVms();
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}

/// Rename a domain. libvirt can only rename shut-off persistent guests, so
/// the daemon rejects a running one and the caller surfaces that reason.
async function renameVm(name: string) {
  if (!selectedConn) return;
  const next = await askText("Rename VM", "New name", name, "alpine-worker");
  if (!next || next === name) return;
  try {
    await api.post(`/proxy/${selectedConn}/domains/${encodeURIComponent(name)}/rename`, { name: next });
    toast("Renamed");
    // Every view keys off the old name, so reselect under the new one and
    // refresh the tree cache.
    if (selectedVm === name) selectedVm = next;
    await refreshTreeVms();
    if (selectedVm) renderVm();
    else renderConn();
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}

async function deleteNetwork(name: string) {
  if (!selectedConn) return;
  if (!(await askConfirm("Delete network", `Delete network "${name}"?`, "Delete", true))) return;
  try {
    await api.del(`/proxy/${selectedConn}/networks/${encodeURIComponent(name)}`);
    toast("Deleted");
    loadNetworks();
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}

async function deletePool(name: string) {
  if (!selectedConn) return;
  if (!(await askConfirm("Delete pool", `Delete storage pool "${name}"?`, "Delete", true))) return;
  try {
    await api.del(`/proxy/${selectedConn}/pools/${encodeURIComponent(name)}`);
    toast("Deleted");
    loadPools();
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}

async function deleteVolume(pool: string, name: string) {
  if (!selectedConn) return;
  if (!(await askConfirm("Delete volume", `Delete volume "${name}"?`, "Delete", true))) return;
  try {
    await api.del(`/proxy/${selectedConn}/pools/${encodeURIComponent(pool)}/volumes/${encodeURIComponent(name)}`);
    toast("Deleted");
    loadVolumes(pool);
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}

async function deleteConnection(id: string) {
  const c = connections.find((x) => x.id === id);
  if (!c) return;
  if (!(await askConfirm("Remove server", `Remove server "${c.name}"?`, "Remove", true))) return;
  try {
    await api.del(`/connections/${id}`);
    connections = await api.get<Connection[]>("/connections");
    if (selectedConn === id) selectedConn = null;
    renderTree();
    // Whatever is on screen: removing a daemon from the admin page stays there.
    renderView();
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}

// ---------------------------------------------------------------- dialogs

function openModal(title: string, body: string, foot: string) {
  const el = document.createElement("div");
  el.className = "modal-back open";
setHtml(el, `
    <div class="modal">
      <div class="modal-head"><span>${title}</span><button class="x">${ic.close()}</button></div>
      <div class="modal-body">${body}</div>
      <div class="modal-foot">${foot}</div>
    </div>`);
  document.body.appendChild(el);
  (el.querySelector(".x") as HTMLElement).onclick = () => el.remove();
  el.onclick = (e) => { if (e.target === el) el.remove(); };
  // The global Escape handler dispatches this; askText/askConfirm add their
  // own listener to resolve their promise, which also removes the element.
  el.addEventListener("modal-escape", () => el.remove());
  // Deferred: callers fill in fields and wire handlers after this returns.
  const opener = document.activeElement as HTMLElement | null;
  queueMicrotask(() => { if (el.isConnected) enterLayer(el, opener); });
  return el;
}

/// Themed replacement for window.prompt — the native dialog ignores the app
/// theme and looks alien inside the frameless window.
function askText(title: string, label: string, value = "", placeholder = ""): Promise<string | null> {
  return new Promise((resolve) => {
    let done = false;
    const finish = (v: string | null) => {
      if (done) return;
      done = true;
      el.remove();
      resolve(v);
    };
    const el = openModal(
      title,
      `<label class="field"><span>${esc(label)}</span>
         <input id="ask-input" value="${esc(value)}" placeholder="${esc(placeholder)}"></label>`,
      `<button class="btn" id="ask-cancel">Cancel</button>
       <button class="btn primary" id="ask-ok">OK</button>`,
    );
    const input = $("#ask-input", el);
    $("#ask-ok", el).onclick = () => finish(input.value);
    $("#ask-cancel", el).onclick = () => finish(null);
    (el.querySelector(".x") as HTMLElement).onclick = () => finish(null);
    el.onclick = (e: Event) => { if (e.target === el) finish(null); };
    el.addEventListener("modal-escape", () => finish(null));
    input.onkeydown = (e: KeyboardEvent) => {
      if (e.key === "Enter") finish(input.value);
      if (e.key === "Escape") finish(null);
    };
    input.focus();
    input.select();
  });
}

/// Themed replacement for window.confirm.
function askConfirm(title: string, message: string, okLabel = "Confirm", danger = false): Promise<boolean> {
  return new Promise((resolve) => {
    let done = false;
    const finish = (v: boolean) => {
      if (done) return;
      done = true;
      el.remove();
      resolve(v);
    };
    const el = openModal(
      title,
      `<p class="hint" style="font-size:12px;color:var(--fg)">${esc(message)}</p>`,
      `<button class="btn" id="ask-no">Cancel</button>
       <button class="btn ${danger ? "danger" : "primary"}" id="ask-yes">${esc(okLabel)}</button>`,
    );
    $("#ask-yes", el).onclick = () => finish(true);
    $("#ask-no", el).onclick = () => finish(false);
    (el.querySelector(".x") as HTMLElement).onclick = () => finish(false);
    el.onclick = (e: Event) => { if (e.target === el) finish(false); };
    el.addEventListener("modal-escape", () => finish(false));
  });
}

/// Add a server, or edit an existing one when `editId` is given. Daemons
/// autoconnect, so editing (not a manual connection test) is the useful
/// per-row action.
function openConnectionDialog(editId?: string) {
  const existing = editId ? connections.find((c) => c.id === editId) : undefined;
  const el = openModal(
    existing ? `Edit Server — ${esc(existing.name)}` : "Add Server",
    `
    <label class="field"><span>Name</span><input id="conn-name" value="${esc(existing?.name ?? "")}"></label>
    <label class="field"><span>URL (https://host:8443)</span><input id="conn-url" placeholder="https://host:8443" value="${esc(existing?.url ?? "")}"></label>
    <label class="field"><span>Auth Token</span>
      <input id="conn-token" placeholder="${existing ? "leave blank to keep the current token" : "paste existing token or generate"}">
      <label class="check"><input type="checkbox" id="conn-gen"> generate new token</label>
    </label>
    <p class="hint" id="conn-fp">${existing?.fingerprint ? `Pinned certificate: ${esc(existing.fingerprint)}` : ""}</p>`,
    `<button class="btn" id="conn-save">Save</button>`,
  );
  $("#conn-save", el).onclick = async () => {
    try {
      const body: any = {
        name: $("#conn-name", el).value,
        url: $("#conn-url", el).value.trim(),
        generate_token: $("#conn-gen", el).checked,
      };
      const typedToken = $("#conn-token", el).value;
      // On edit an empty token means "keep the current one"; the backend
      // leaves it untouched when the field is absent.
      if (!body.generate_token && (typedToken || !existing)) body.token = typedToken;

      if (existing) {
        await api.put(`/connections/${existing.id}`, body);
        toast("Server updated");
        el.remove();
        await reloadConnections();
        return;
      }

      const created = await api.post<CreateConnectionResult>("/connections", body);
      el.remove();
      await reloadConnections();
      if (created.daemon_unreachable) {
        const statusEl = offerInstallScript(created.install_script ?? "");
        void pollUntilCertPinned(created.id, statusEl);
        return;
      }
      toast("Server added");
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };
}

async function reloadConnections() {
  connections = await api.get<Connection[]>("/connections");
  renderTree();
  if (!selectedConn) renderView();
  void refreshTreeVms();
}

/// Show the generated installer for a daemon that was saved while unreachable.
/// Returns the status line element so the poller can update it in place.
function offerInstallScript(script: string): HTMLElement | null {
  const el = openModal(
    "Daemon not reachable",
    `
    <label class="field"><span>Install Script</span>
      <textarea id="install-script" rows="14" readonly spellcheck="false">${esc(script)}</textarea>
    </label>
    <p class="hint" id="pending-status">Waiting for the daemon…</p>`,
    `<button class="btn" id="dl-script">Download Script</button>`,
  );
  $("#dl-script", el).onclick = () => {
    const blob = new Blob([script], { type: "application/x-sh" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = "install-virt-man-daemon.sh";
    a.click();
    URL.revokeObjectURL(url);
  };
  return $("#pending-status", el);
}

/// Poll a pending daemon every second until its cert can be fetched, then pin
/// it. The generated install script may still be running.
function pollUntilCertPinned(connId: string, statusEl: HTMLElement | null) {
  stopPendingPoll();
  pendingPoll = window.setInterval(async () => {
    try {
      const r = await api.get<{ cert: string; fingerprint: string }>(`/connections/${connId}/cert`);
      stopPendingPoll();
      const conn = connections.find((c) => c.id === connId);
      if (conn) await api.put(`/connections/${connId}`, { name: conn.name, url: conn.url, cert: r.cert });
      toast("Daemon connected — certificate pinned");
      await reloadConnections();
    } catch {
      if (statusEl) statusEl.textContent = "Waiting for the daemon…";
    }
  }, 1000);
}

function stopPendingPoll() {
  if (pendingPoll !== null) {
    window.clearInterval(pendingPoll);
    pendingPoll = null;
  }
}

async function loadFormData(conn: string) {
  const pools = await api.get<PoolSummary[]>(`/proxy/${conn}/pools`);
  const nets = await api.get<NetworkSummary[]>(`/proxy/${conn}/networks`);
  return { pools, nets };
}

/// Create a volume for every "new volume" disk in the device list, then rewrite
/// the device to reference it, so the XML builder can attach it like an
/// existing volume. Called before defining the domain in both create and edit.
async function createPendingVolumes(
  conn: string,
  vmName: string,
  devices: HwDevice[],
  set: (msg: string) => void,
  made: { pool: string; name: string }[] = [],
) {
  for (let i = 0; i < devices.length; i++) {
    const d = devices[i];
    if (d.kind !== "disk" || (d.fields.mode ?? "existing") !== "new") continue;
    const pool = (d.fields.pool ?? "").trim();
    const sizeGb = Math.max(1, +(d.fields.size_gb ?? "") || 1);
    const format = (d.fields.format ?? "qcow2") === "raw" ? "raw" : "qcow2";
    const name = `${vmName}-disk${i + 1}-${sizeGb}gb.${format}`;
    set(`creating ${sizeGb} GB volume ${name}`);
    const vol = await api.post<{ name: string; path: string }>(
      `/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes`,
      { xml: volumeXml({ name, capacity_gb: sizeGb, format }),
    });
    made.push({ pool, name });
    d.fields.volume = name;
    // Address it by path as well: the reference form is not resolvable on
    // every libvirt, and an unresolvable disk keeps the guest from starting.
    if (vol.path) d.fields.path = vol.path;
    delete d.fields.mode;
  }
}

/// Proxmox-style hardware editor: device list on the LEFT, the selected
/// device's form on the right. Used for both "Create VM" and editing an
/// existing guest's hardware, so the two can never drift apart.
async function openHardwareEditor(opts: {
  vm?: string;
  inline?: boolean;
  /// Create on this server instead of the selected one (the admin page).
  conn?: string;
  /// An admin creating the guest for someone else: who gets it, and what to
  /// run once it exists (repaint the admin page, which stays open).
  forUser?: { owner: string; after: () => void };
} = {}) {
  const conn = opts.conn ?? selectedConn;
  if (!conn) return;
  const editing = !!opts.vm;
  const owner = editing ? null : opts.forUser?.owner ?? null;
  const inline = !!opts.inline;

  // Existing guest: read its current definition so the form starts populated.
  let baseXml = "";
  let devices: HwDevice[] = [];
  let general = {
    name: "",
    cpus: 2,
    memory_mb: 2048,
    firmware: "uefi" as "uefi" | "bios",
    graphics: "vnc" as "vnc" | "spice",
  };

  if (editing && opts.vm) {
    try {
      const { xml } = await api.get<{ xml: string }>(
        `/proxy/${conn}/domains/${encodeURIComponent(opts.vm)}/xml`);
      baseXml = xml;
      devices = devicesFromXml(xml);
      general = {
        name: opts.vm,
        cpus: +(xml.match(/<vcpu[^>]*>(\d+)<\/vcpu>/)?.[1] ?? 2),
        memory_mb: Math.round(+(xml.match(/<memory[^>]*>(\d+)<\/memory>/)?.[1] ?? 2097152) / 1024),
        firmware: /pflash|OVMF/i.test(xml) ? "uefi" : "bios",
        graphics: /<graphics[^>]*type=['"]spice/.test(xml) ? "spice" : "vnc",
      };
    } catch (e) {
      toast((e as Error).message, "bad");
      return;
    }
  }

  // New VM: one disk is created up front, described by these fields.
  // The boot disk: a new volume, or one the user already has ("existing").
  const newDisk = {
    pool: "", size_gb: 10, format: "qcow2" as "raw" | "qcow2",
    mode: "new" as "new" | "existing", volume: "",
  };

  // Create VM keeps its modal; editing an existing guest renders the editor
  // inline in the Hardware tab, so devices are editable without an extra
  // "Edit hardware" click.
  const modal = inline ? null : openModal(
    editing ? `Hardware — ${esc(opts.vm ?? "")}` : "Create VM",
    "",
    `<button class="btn" id="hw-cancel">Cancel</button>
     <button class="btn primary" id="hw-save">${editing ? "Save" : "Create"}</button>`,
  );
  if (modal) modal.querySelector(".modal")?.classList.add("hw-modal");

  const { pools, nets } = await loadFormData(conn);
  // Volumes per pool, fetched lazily and cached for the device forms.
  const volCache: Record<string, VolumeSummary[]> = {};
  const volumes = async (pool: string): Promise<VolumeSummary[]> => {
    if (!pool) return [];
    if (!volCache[pool]) {
      try {
        volCache[pool] = await api.get<VolumeSummary[]>(
          `/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes`);
      } catch {
        volCache[pool] = [];
      }
    }
    return volCache[pool];
  };
  /// Point a disk at its volume's path, so the definition refers to the file
  /// rather than to a pool+volume pair. libvirt cannot resolve a volume source
  /// on every host it ships on, and a disk it cannot resolve never reaches the
  /// guest's AppArmor profile — so qemu is refused permission and the guest
  /// will not start. Cleared when the volume is unknown, which keeps the
  /// reference form for a definition we cannot account for.
  const resolveDevicePath = async (d: HwDevice) => {
    const name = (d.fields.volume ?? "").trim();
    const pool = (d.fields.pool ?? "").trim();
    const found = name ? (await volumes(pool)).find((v) => v.name === name) : undefined;
    if (!found?.path) {
      delete d.fields.path;
      return;
    }
    d.fields.path = found.path;
    // The volume list carries no format, so it comes from the name: we name
    // our own volumes after their format, and getting this wrong would make
    // qemu parse the file as the wrong image type.
    d.fields.format = /\.raw$/i.test(found.name) ? "raw" : "qcow2";
  };
  // Pools that contain at least one ISO. The cdrom pool dropdown hides
  // every other pool, so a pool with no bootable media never appears.
  let isoPoolsCache: string[] | null = null;
  const isoPools = async (): Promise<string[]> => {
    if (isoPoolsCache) return isoPoolsCache;
    const out: string[] = [];
    for (const p of pools) {
      if ((await volumes(p.name)).some((v) => v.name.toLowerCase().endsWith(".iso"))) {
        out.push(p.name);
      }
    }
    isoPoolsCache = out;
    return out;
  };
  newDisk.pool = pools[0]?.name ?? "";

  // New VMs start with an empty CD/DVD drive and a network NIC, so both a
  // boot ISO and connectivity are one click away instead of buried in
  // Add Hardware. The NIC defaults to the first network.
  if (!editing) {
    devices = [{ kind: "cdrom", fields: { pool: newDisk.pool, volume: "" } }];
    if (nets.length) {
      devices.push({ kind: "interface", fields: { network: nets[0].name, model: "virtio" } });
    }
  }

  // Where the editor paints. Inline mode reuses the Hardware tab body; the
  // footer buttons live outside the re-rendered region so their handlers
  // survive field edits (render() replaces only the .hw-body area).
  let host: HTMLElement;
  let bodyHost: HTMLElement;
  if (inline) {
    host = $("#vm-body") as HTMLElement;
    // Let the editor fill the pane below the tab bar.
    host.parentElement?.classList.add("vm-fill");
    setHtml(host, `<div class="hw-inline">
      <div class="hw-inline-body"></div>
      <div class="hw-foot">
        <button class="btn" id="hw-cancel">Discard</button>
        <button class="btn primary" id="hw-save">Save</button>
      </div>
    </div>`);
    bodyHost = host.querySelector(".hw-inline-body") as HTMLElement;
  } else {
    host = modal!.querySelector(".modal-body") as HTMLElement;
    bodyHost = host;
    host.style.padding = "0";
    host.style.overflow = "hidden";
  }
  // The foot buttons live in `root` (the modal wrapper or the inline host).
  const root = (inline ? host : modal!) as HTMLElement;

  let page = "general";

  const opt = (list: string[], sel: string) =>
    list.map((v) => `<option value="${esc(v)}" ${v === sel ? "selected" : ""}>${esc(v)}</option>`).join("");

  /** Sidebar rows mirror Proxmox: label plus the current value. */
  function sidebar(): string {
    const fixed = [
      { key: "general", icon: ic.datacenter(), label: "General", sum: general.name || "unnamed" },
      { key: "processors", icon: ic.cpu(), label: "Processors", sum: `${general.cpus} vCPU` },
      { key: "memory", icon: ic.memory(), label: "Memory", sum: `${general.memory_mb} MB` },
      ...(editing ? [] : [{
        key: "disk", icon: ic.disk(), label: "Hard Disk",
        sum: newDisk.mode === "existing" ? (newDisk.volume || "choose a volume") : `${newDisk.size_gb} GB ${newDisk.format}`,
      }]),
      { key: "display", icon: ic.vm(), label: "Display", sum: general.graphics },
      { key: "firmware", icon: ic.cpu(), label: "BIOS", sum: general.firmware === "uefi" ? "UEFI (OVMF)" : "BIOS (SeaBIOS)" },
    ];
    const rows = fixed.map((f) => `
      <button class="hw-item ${page === f.key ? "active" : ""}" data-hw="${f.key}">
        <span class="hw-ic">${f.icon}</span>
        <span class="hw-tx"><b>${esc(f.label)}</b><i>${esc(f.sum)}</i></span>
      </button>`).join("");
    const custom = devices.map((d, i) => {
      const t = hwType(d.kind);
      return `
      <button class="hw-item hw-custom ${page === `dev-${i}` ? "active" : ""}" data-hw="dev-${i}">
        <span class="hw-ic">${ic[t?.icon ?? "cpu"]()}</span>
        <span class="hw-tx"><b>${esc(t?.label ?? d.kind)}</b><i>${esc(deviceSummary(d))}</i></span>
      </button>`;
    }).join("");
    return `
      <div class="hw-side-head">HARDWARE</div>
      <div class="hw-side-scroll">${rows}<div class="hw-custom-list">${custom}</div></div>
      <button class="hw-add" id="hw-add">${ic.plus(15)} ADD HARDWARE</button>`;
  }

  async function pageHtml(): Promise<string> {
    if (page.startsWith("dev-")) {
      const i = +page.slice(4);
      const d = devices[i];
      const t = hwType(d.kind);
      if (!t) return `<div class="hw-page active"><div class="hw-note">Unknown device.</div></div>`;
      // A cdrom whose pool has no ISOs points at the first pool that does,
      // so its volume list is never silently empty.
      if (d.kind === "cdrom") {
        const withIsos = await isoPools();
        if (withIsos.length && !withIsos.includes(d.fields.pool ?? "")) {
          d.fields.pool = withIsos[0];
        }
      }
      // After the pool is settled, so the path is looked up in the right one.
      if (d.kind === "disk" || d.kind === "cdrom") await resolveDevicePath(d);
      const fields = await Promise.all(t.fields.map(async (f) => {
        const val = d.fields[f.key] ?? f.value ?? "";
        if (f.type === "select") {
          let list: string[] = (f.options ?? []).map((o) => o.value);
          if (f.key === "pool") list = d.kind === "cdrom" ? await isoPools() : pools.map((p) => p.name);
          if (f.key === "network") list = nets.map((n) => n.name);
          if (f.key === "volume") {
            list = (await volumes(d.fields.pool ?? pools[0]?.name ?? "")).map((v) => v.name);
            // The cdrom picks an ISO; a disk may pick any volume.
            if (d.kind === "cdrom") list = list.filter((v) => v.toLowerCase().endsWith(".iso"));
          }
          // A cdrom may be empty: that is a valid empty drive.
          const emptyOpt = d.kind === "cdrom" && f.key === "volume"
            ? `<option value="" ${val ? "" : "selected"}>No media</option>`
            : "";
          const labels = f.options?.length
            ? f.options.map((o) => `<option value="${esc(o.value)}" ${o.value === val ? "selected" : ""}>${esc(o.label)}</option>`).join("")
            : emptyOpt + opt(list, val);
          return `<label class="field"><span>${esc(f.label)}</span>
            <select data-field="${esc(f.key)}">${labels}</select></label>`;
        }
        return `<label class="field"><span>${esc(f.label)}</span>
          <input data-field="${esc(f.key)}" type="${f.type}" value="${esc(val)}"
            placeholder="${esc(f.placeholder ?? "")}"></label>`;
      }));
      return `<div class="hw-page active">
        <h3 class="hw-title">${esc(t.label)}</h3>
        ${fields.join("")}
        <div class="actions"><button class="btn danger" id="hw-remove">${ic.trash(14)} Remove device</button></div>
      </div>`;
    }

    switch (page) {
      case "processors":
        return `<div class="hw-page active">
          <h3 class="hw-title">Processors</h3>
          <label class="field"><span>vCPUs</span>
            <input id="g-cpus" type="number" min="1" max="128" value="${general.cpus}"></label>
        </div>`;
      case "memory":
        return `<div class="hw-page active">
          <h3 class="hw-title">Memory</h3>
          <label class="field"><span>Memory (MB)</span>
            <input id="g-mem" type="number" min="128" step="256" value="${general.memory_mb}"></label>
        </div>`;
      case "disk": {
        // Volumes the user may boot from: their own, and not already another
        // guest's disk (one writable disk in two guests corrupts both). An
        // admin sees every free volume. ISOs are media, not boot disks.
        const all = await volumes(newDisk.pool);
        const usable = all.filter((v) => !v.name.toLowerCase().endsWith(".iso")
          && (canAdmin() || v.mine) && !(v.used_by?.length));
        const held = all.filter((v) => (canAdmin() || v.mine) && v.used_by?.length);
        if (newDisk.mode === "existing" && !usable.some((v) => v.name === newDisk.volume)) {
          newDisk.volume = usable[0]?.name ?? "";
        }
        const chosen = usable.find((v) => v.name === newDisk.volume);
        const mode = `<label class="field"><span>Disk</span><select id="g-disk-mode">
            <option value="new" ${newDisk.mode === "new" ? "selected" : ""}>Create a new volume</option>
            <option value="existing" ${newDisk.mode === "existing" ? "selected" : ""}>Use one of my volumes</option>
          </select></label>`;
        const pool = `<label class="field"><span>Storage Pool</span>
            <select id="g-pool">${opt(pools.map((p) => p.name), newDisk.pool)}</select></label>`;
        if (newDisk.mode === "existing") {
          return `<div class="hw-page active">
            <h3 class="hw-title">Hard Disk</h3>
            ${mode}${pool}
            ${usable.length ? `<label class="field"><span>Volume</span><select id="g-volume">
                ${usable.map((v) => `<option value="${esc(v.name)}" ${v.name === newDisk.volume ? "selected" : ""}>
                  ${esc(`${v.name} — ${fmtBytes(v.capacity)}`)}</option>`).join("")}
              </select></label>
              ${chosen && !chosen.blank && !canAdmin()
                ? `<p class="hint">Attached as raw: only disks created empty here may be used as qcow2.</p>` : ""}`
              : `<p class="hint">You have no free volumes in this pool${held.length ? " (the ones you have are already attached to a guest)" : ""}.
                  Create one from the pool's page, or choose "Create a new volume".</p>`}
            ${held.length ? `<p class="hint">In use, not offered: ${held.map((v) => esc(`${v.name} (${v.used_by!.join(", ")})`)).join("; ")}.</p>` : ""}
          </div>`;
        }
        return `<div class="hw-page active">
          <h3 class="hw-title">Hard Disk</h3>
          ${mode}${pool}
          <label class="field"><span>Disk Size (GB)</span>
            <input id="g-size" type="number" min="1" value="${newDisk.size_gb}"></label>
          <label class="field"><span>Format</span>
            <select id="g-format">${opt(["qcow2", "raw"], newDisk.format)}</select></label>
        </div>`;
      }
      case "display":
        return `<div class="hw-page active">
          <h3 class="hw-title">Display</h3>
          <label class="field"><span>Graphics</span>
            <select id="g-gfx">${opt(["vnc", "spice"], general.graphics)}</select></label>
        </div>`;
      case "firmware":
        return `<div class="hw-page active">
          <h3 class="hw-title">BIOS</h3>
          <label class="field"><span>Firmware</span>
            <select id="g-fw">${opt(["uefi", "bios"], general.firmware)}</select></label>
        </div>`;
      default:
        return `<div class="hw-page active">
          <h3 class="hw-title">General</h3>
          <label class="field"><span>Name</span>
            <input id="g-name" value="${esc(general.name)}" placeholder="arch-box"></label>
        </div>`;
    }
  }

  async function render() {
    // The page first: it can settle choices the sidebar summarises (the disk
    // page picks a default volume), and a template literal evaluates the
    // sidebar before awaiting the page otherwise.
    const pageBody = await pageHtml();
    setHtml(bodyHost, `<div class="hw-body">
      <nav class="hw-side" id="hw-side">${sidebar()}</nav>
      <div class="hw-pane" id="hw-pane">${pageBody}</div>
    </div>`);
    if (!inline) {
      (bodyHost as HTMLElement).style.padding = "0";
      (bodyHost as HTMLElement).style.overflow = "hidden";
    }

    root.querySelectorAll("[data-hw]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => {
        page = (b as HTMLElement).dataset.hw!;
        void render();
      };
    });
    $("#hw-add", root).onclick = () => openAddHardware();

    // General/spec fields write straight back into the model.
    const bind = (sel: string, fn: (v: string) => void) => {
      const n = $(sel, root);
      if (!n) return;
      n.onchange = () => { fn(n.value); void render(); };
    };
    bind("#g-name", (v) => { general.name = v; });
    bind("#g-cpus", (v) => { general.cpus = Math.max(1, +v || 1); });
    bind("#g-mem", (v) => { general.memory_mb = Math.max(128, +v || 128); });
    bind("#g-gfx", (v) => { general.graphics = v as "vnc" | "spice"; });
    bind("#g-fw", (v) => { general.firmware = v as "uefi" | "bios"; });
    bind("#g-pool", (v) => { newDisk.pool = v; newDisk.volume = ""; });
    bind("#g-disk-mode", (v) => { newDisk.mode = v === "existing" ? "existing" : "new"; });
    bind("#g-volume", (v) => { newDisk.volume = v; });
    bind("#g-size", (v) => { newDisk.size_gb = Math.max(1, +v || 1); });
    bind("#g-format", (v) => { newDisk.format = v as "raw" | "qcow2"; });

    // Device field edits, plus removal.
    if (page.startsWith("dev-")) {
      const i = +page.slice(4);
      root.querySelectorAll("[data-field]").forEach((n: Element) => {
        (n as HTMLElement).onchange = () => {
          const key = (n as HTMLElement).dataset.field!;
          devices[i].fields[key] = (n as HTMLInputElement).value;
          // Changing the pool invalidates the volume list.
          void render();
        };
      });
      const rm = $("#hw-remove", root);
      if (rm) rm.onclick = () => {
        devices.splice(i, 1);
        page = "general";
        void render();
      };
    }
  }

  /** Chooser for a new device; fields come from the catalogue. */
  function openAddHardware() {
    let kind = HW_CATALOG[0].id;
    const draft: Record<string, string> = {};
    // Single-instance devices (memballoon, watchdog, tpm, …) already present
    // cannot be added again: libvirt rejects the duplicate at define time.
    const alreadyAdded = (id: string) =>
      !!hwType(id)?.single && devices.some((d) => d.kind === id);

    const add = openModal("Add Hardware", "",
      `<button class="btn" id="ah-cancel">Cancel</button>
       <button class="btn primary" id="ah-save">Add</button>`);
    add.querySelector(".modal")?.classList.add("add-hw-modal");

    const renderAdd = async () => {
      const t = hwType(kind)!;
      const fieldValue = (key: string) => {
        if (draft[key] !== undefined) return draft[key];
        return t.fields.find((x) => x.key === key)?.value ?? "";
      };
      // showWhen keeps the form to only the fields relevant to the current
      // mode (e.g. an existing-volume disk shows pool+volume, a new-volume
      // disk shows pool+size+format).
      const visible = t.fields.filter((f) =>
        !f.showWhen || fieldValue(f.showWhen.key) === f.showWhen.value);
      const fields = await Promise.all(visible.map(async (f) => {
        const val = draft[f.key] ?? f.value ?? "";
        if (f.type === "select") {
          let list: string[] = (f.options ?? []).map((o) => o.value);
          if (f.key === "pool") list = kind === "cdrom" ? await isoPools() : pools.map((p) => p.name);
          if (f.key === "network") list = nets.map((n) => n.name);
          if (f.key === "volume") {
            const pool = kind === "cdrom"
              ? (draft.pool ?? (await isoPools())[0] ?? "")
              : (draft.pool ?? pools[0]?.name ?? "");
            list = (await volumes(pool)).map((v) => v.name);
            // The cdrom picks an ISO; a disk may pick any volume.
            if (kind === "cdrom") list = list.filter((v) => v.toLowerCase().endsWith(".iso"));
          }
          // A cdrom may be empty: that is a valid empty drive.
          const emptyOpt = kind === "cdrom" && f.key === "volume"
            ? `<option value="" ${val ? "" : "selected"}>No media</option>`
            : "";
          const labels = f.options?.length
            ? f.options.map((o) => `<option value="${esc(o.value)}" ${o.value === val ? "selected" : ""}>${esc(o.label)}</option>`).join("")
            : emptyOpt + opt(list, val);
          return `<label class="field"><span>${esc(f.label)}</span>
            <select data-af="${esc(f.key)}">${labels}</select></label>`;
        }
        return `<label class="field"><span>${esc(f.label)}</span>
          <input data-af="${esc(f.key)}" type="${f.type}" value="${esc(val)}"
            placeholder="${esc(f.placeholder ?? "")}"></label>`;
      }));

      const preview = deviceXml({ kind, fields: draft }, devices.length + 1) || "—";
      setHtml(add.querySelector(".modal-body")!, `<div class="add-hw-body">
        <aside class="add-hw-types">
          ${HW_CATALOG.map((t2) => {
            const dup = alreadyAdded(t2.id);
            return `
            <button class="add-hw-type ${t2.id === kind ? "active" : ""} ${dup ? "dup" : ""}" data-at="${t2.id}" ${dup ? "disabled" : ""}>
              <b>${esc(t2.label)}</b><small>${dup ? "Already added" : esc(t2.blurb)}</small>
            </button>`;
          }).join("")}
        </aside>
        <div class="add-hw-form">
          <div class="hw-title">${esc(t.label)}</div>
          <div class="add-hw-fields">${fields.join("")}</div>
          <div class="field add-hw-preview"><span>LIBVIRT XML PREVIEW</span>
            <code>${esc(preview)}</code></div>
        </div>
      </div>`);
      (add.querySelector(".modal-body") as HTMLElement).style.padding = "0";
      (add.querySelector(".modal-body") as HTMLElement).style.overflow = "hidden";

      add.querySelectorAll("[data-at]").forEach((b: Element) => {
        (b as HTMLElement).onclick = () => {
          kind = (b as HTMLElement).dataset.at!;
          for (const k of Object.keys(draft)) delete draft[k];
          void renderAdd();
        };
      });
      add.querySelectorAll("[data-af]").forEach((n: Element) => {
        (n as HTMLElement).onchange = () => {
          draft[(n as HTMLElement).dataset.af!] = (n as HTMLInputElement).value;
          void renderAdd();
        };
      });
      $("#ah-cancel", add).onclick = () => add.remove();
      $("#ah-save", add).onclick = () => {
        // A duplicate single-instance device would fail at define time with
        // e.g. "only a single memory balloon device is supported".
        if (alreadyAdded(kind)) {
          return toast(`${hwType(kind)!.label} is already added`, "bad");
        }
        // Seed any untouched select with its first rendered option, so a
        // device added without interaction is still valid.
        add.querySelectorAll("[data-af]").forEach((n: Element) => {
          const key = (n as HTMLElement).dataset.af!;
          if (draft[key] === undefined) draft[key] = (n as HTMLInputElement).value;
        });
        const device: HwDevice = { kind, fields: { ...draft } };
        const bad = validateDevice(device);
        if (bad) return toast(bad, "bad");
        devices.push(device);
        add.remove();
        page = `dev-${devices.length - 1}`;
        void render();
      };
    };
    void renderAdd();
  }

  $("#hw-cancel", root).onclick = () => {
    if (modal) modal.remove();
    else void renderVm();
  };
  $("#hw-save", root).onclick = async () => {
    for (const d of devices) {
      const bad = validateDevice(d);
      if (bad) return toast(bad, "bad");
    }

    if (editing && opts.vm) {
      const oldName = opts.vm;
      const renamed = general.name.trim() && general.name.trim() !== oldName;
      // Close only the modal; the inline editor stays visible and the
      // post-task renderVm() below refreshes it from the new definition.
      modal?.remove();
      void runTask(`Update ${general.name.trim() || oldName}`, async (set) => {
        let target = oldName;
        if (renamed) {
          // Rename through the dedicated endpoint: it swaps the persistent
          // definition while keeping the UUID. A name baked into the XML PUT
          // below would instead define a second domain with a clashing UUID.
          set(`renaming ${oldName} → ${general.name.trim()}`);
          await api.post(
            `/proxy/${conn}/domains/${encodeURIComponent(oldName)}/rename`,
            { name: general.name.trim() });
          target = general.name.trim();
          if (selectedVm === oldName) selectedVm = target;
        }
        set("rebuilding domain definition");
        // Create any "new volume" disks before the XML references them.
        await createPendingVolumes(conn, target, devices, set);
        // Strip the devices this editor owns, then splice the current set back
        // in, so libvirt-added devices we do not model survive the round trip.
        const stripped = baseXml
          .replace(/<disk\b[^>]*device=['"](disk|cdrom)['"][\s\S]*?<\/disk>\s*/g, "")
          .replace(/<interface\b[^>]*type=['"]network['"][\s\S]*?<\/interface>\s*/g, "")
          .replace(/<rng\b[\s\S]*?<\/rng>\s*/g, "")
          // Both self-closing and child-bearing shapes must be stripped:
          // libvirt writes <memballoon model='virtio'> with an <address>
          // child, and <watchdog> as self-closing, so matching only one shape
          // leaves the other behind and the splice re-adds a duplicate.
          .replace(/<memballoon\b(?:[^>]*\/>|[\s\S]*?<\/memballoon>)\s*/g, "")
          .replace(/<channel\b[\s\S]*?org\.qemu\.guest_agent[\s\S]*?<\/channel>\s*/g, "")
          .replace(/<tpm\b[\s\S]*?<\/tpm>\s*/g, "")
          .replace(/<watchdog\b(?:[^>]*\/>|[\s\S]*?<\/watchdog>)\s*/g, "")
          .replace(/<vcpu([^>]*)>\d+<\/vcpu>/, `<vcpu$1>${general.cpus}</vcpu>`)
          .replace(/<memory([^>]*)>\d+<\/memory>/, `<memory$1>${general.memory_mb * 1024}</memory>`)
          .replace(/<currentMemory([^>]*)>\d+<\/currentMemory>/, `<currentMemory$1>${general.memory_mb * 1024}</currentMemory>`)
          .replace(/<name>[^<]*<\/name>/, `<name>${esc(target)}</name>`);
        set("defining domain");
        await api.putXml(`/proxy/${conn}/domains/${encodeURIComponent(target)}/xml`,
          withFirmware(withDevices(stripped, devices), general.firmware, target));
        await refreshTreeVms();
      }, { done: "Hardware saved", notify: toast }).then(() => void renderVm());
      return;
    }

    if (!general.name.trim()) return toast("Name is required", "bad");
    // Caught here, with the form still open, rather than after it closes.
    if (newDisk.mode === "existing" && !newDisk.volume) {
      page = "disk";
      void render();
      return toast("Choose a volume for the hard disk, or create a new one", "bad");
    }
    const form: DomainForm = {
      name: general.name.trim(),
      cpus: general.cpus,
      memory_mb: general.memory_mb,
      firmware: general.firmware,
      graphics: general.graphics,
      network: "",
      bridge: "",
      pool: newDisk.pool,
      new_disk: false,
      existing_volume: `${general.name.trim()}.qcow2`,
      disk_size_gb: newDisk.size_gb,
      disk_format: newDisk.format,
      iso: "",
      iso_pool: newDisk.pool,
    };
    modal?.remove();
    // Volumes made on the way, deleted again if the guest is not defined:
    // otherwise a refused create (quota, permission) leaves a disk behind that
    // still counts against the user's storage.
    const made: { pool: string; name: string }[] = [];
    void runTask(`Create VM ${form.name}`, async (set) => {
      try {
      let disk: { name: string; path: string };
      if (newDisk.mode === "existing") {
        // One of the user's own volumes, by path like a new one. The format
        // follows what the server will accept: qcow2 only for a volume made
        // empty here, since an uploaded qcow2 can name any host file as its
        // backing store; anything else goes on as raw.
        const v = (await volumes(newDisk.pool)).find((x) => x.name === newDisk.volume);
        if (!v) throw new Error("choose a volume for the hard disk, or create a new one");
        disk = { name: v.name, path: v.path };
        const qcow = v.name.toLowerCase().endsWith(".qcow2");
        form.disk_format = qcow && (v.blank || canAdmin()) ? "qcow2" : "raw";
      } else {
        set(`creating ${newDisk.size_gb} GB volume`);
        disk = await api.post<{ name: string; path: string }>(
          `/proxy/${conn}/pools/${encodeURIComponent(newDisk.pool)}/volumes`,
          { xml: volumeXml({ name: `${form.name}.qcow2`, capacity_gb: newDisk.size_gb, format: newDisk.format }) });
        // Only a volume made here is deleted again if the create is refused;
        // a chosen one was the user's before and stays theirs.
        made.push({ pool: newDisk.pool, name: disk.name });
      }
      // Refer to the disk by its absolute path rather than by pool+volume: the
      // reference form cannot be resolved by every libvirt, and a disk libvirt
      // cannot resolve never reaches the guest's AppArmor profile, so qemu is
      // refused permission to open it and the guest will not start.
      form.disk_path = disk.path;
      // Create any additional "new volume" disks before defining the domain.
      await createPendingVolumes(conn, form.name, devices, set, made);
      set("defining domain");
      // Defined shut off: creating a guest must not also boot it, so a
      // mis-sized or mis-configured VM cannot start eating resources before
      // it has been reviewed. Start it deliberately from the VM view.
      // No IPAM step: the daemon's DHCP server gives each NIC an address the
      // first time it asks and keeps it until the VM is deleted. (Allocating
      // here rewrote the network definition, which restarted it and
      // disconnected every other guest on it.)
      await api.postXml(`/proxy/${conn}/domains?start=false`, withDevices(domainXml(form), devices));
      } catch (e) {
        for (const v of made) {
          await api.del(`/proxy/${conn}/pools/${encodeURIComponent(v.pool)}/volumes/${encodeURIComponent(v.name)}`)
            .catch(() => undefined);
        }
        throw e;
      }
      if (owner) {
        set("assigning owner");
        await assignOwner(conn, "vm", form.name, owner);
      }
    }, { done: `VM ${form.name} created`, notify: toast })
      .then(() => {
        void refreshTreeVms();
        if (opts.forUser) opts.forUser.after();
        else if (selectedConn === conn && !selectedVm) renderConn();
      });
  };

  void render();
}

/// Back-compat entry point used by the VMs tab button.
function openCreateVm() {
  withDaemon(() => void openHardwareEditor());
}

/// Modal listing the saved daemons; resolves to the chosen connection id, or
/// null when dismissed. Used before creating a guest from the Datacenter.
function pickDaemon(): Promise<string | null> {
  return new Promise((resolve) => {
    if (!connections.length) {
      toast("Add a server first", "bad");
      resolve(null);
      return;
    }
    const el = openModal(
      "Create guest — select daemon",
      `<div class="create-resource-grid">
        ${connections.map((c) => `
          <button class="create-resource-card" data-pick-conn="${c.id}">
            <span class="metric-ic">${ic.server()}</span>
            <b>${esc(c.name)}</b>
            <small>${esc(c.url)}</small>
          </button>`).join("")}
      </div>`,
      `<button class="btn" id="pick-cancel">Cancel</button>`,
    );
    $("#pick-cancel", el).onclick = () => { el.remove(); resolve(null); };
    el.querySelectorAll("[data-pick-conn]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => {
        const id = (b as HTMLElement).dataset.pickConn!;
        el.remove();
        resolve(id);
      };
    });
  });
}

/// The resource kinds offered by both the sidebar's Add Resource button and
/// the server dashboard's Create Resource button. `daemon` adds the one kind
/// that is only creatable from the sidebar (there is no server selected yet).
function resourceCards(opts: { daemon?: boolean } = {}): string {
  const daemon = opts.daemon
    ? `<button class="create-resource-card" data-create="daemon">
        <span class="metric-ic">${ic.server()}</span>
        <b>Daemon</b>
        <small>Connect a hypervisor host by URL and token</small>
      </button>`
    : "";
  return `<div class="create-resource-grid">
      ${daemon}
      <button class="create-resource-card" data-create="vm">
        <span class="metric-ic">${ic.vm()}</span>
        <b>Virtual Machine</b>
        <small>Full hardware editor: CPU, memory, disks and more</small>
      </button>
      <button class="create-resource-card" data-create="network">
        <span class="metric-ic">${ic.network()}</span>
        <b>Network</b>
        <small>NAT, bridge or isolated with DHCP and DNS</small>
      </button>
      <button class="create-resource-card" data-create="pool">
        <span class="metric-ic">${ic.storage()}</span>
        <b>Storage Pool</b>
        <small>Directory, LVM or filesystem-backed pool</small>
      </button>
    </div>`;
}

/// Wire a resource chooser's cards to their create flows. Every kind except
/// the daemon itself needs a daemon, so it is picked first when none is open.
function bindResourceCards(el: Element, close: () => void) {
  el.querySelectorAll("[data-create]").forEach((b: Element) => {
    (b as HTMLElement).onclick = () => {
      const kind = (b as HTMLElement).dataset.create!;
      close();
      if (kind === "daemon") openConnectionDialog();
      else if (kind === "vm") openCreateVm();
      else if (kind === "network") withDaemon(() => openCreateNetwork());
      else withDaemon(() => openCreatePool());
    };
  });
}

/// Run a create flow against a daemon, asking which one when none is selected.
function withDaemon(run: () => void) {
  if (selectedConn) {
    run();
    return;
  }
  void pickDaemon().then((id) => {
    if (!id) return;
    selectedConn = id;
    renderTree();
    run();
  });
}

/// Sidebar "Add Resource": daemon, VM, network or storage pool.
function openAddResource() {
  const el = openModal(
    "Add Resource",
    resourceCards({ daemon: true }),
    `<button class="btn" id="ar-cancel">Cancel</button>`,
  );
  $("#ar-cancel", el).onclick = () => el.remove();
  bindResourceCards(el, () => el.remove());
}

/// Dashboard "Create Resource": pick VM / Network / Storage Pool, then hand
/// off to the existing create flows.
function openCreateResource() {
  if (!selectedConn) return;
  const el = openModal(
    "Create Resource",
    resourceCards(),
    `<button class="btn" id="cr-cancel">Cancel</button>`,
  );
  $("#cr-cancel", el).onclick = () => el.remove();
  bindResourceCards(el, () => el.remove());
}

function openCreatePool() {
  if (!selectedConn) return;
  const el = openModal(
    "Create storage pool",
    `<div class="grid2">
      <label class="field"><span>Name</span><input id="pool-name"></label>
      <label class="field"><span>Type</span><select id="pool-type">
        <option value="dir">Directory</option><option value="logical">LVM</option><option value="fs">Filesystem</option>
      </select></label>
      <label class="field"><span>Target Path</span><input id="pool-target" placeholder="/var/lib/libvirt/images"></label>
      <label class="field"><span>Source (device/dir)</span><input id="pool-source" placeholder="/dev/vg0"></label>
    </div>`,
    `<button class="btn primary" id="pool-create">Create</button>`,
  );
  $("#pool-create", el).onclick = async () => {
    const form: PoolForm = {
      name: $("#pool-name", el).value,
      type: ($("#pool-type", el) as HTMLSelectElement).value as PoolForm["type"],
      target: $("#pool-target", el).value,
      source: $("#pool-source", el).value,
      source_format: "auto",
    };
    if (!form.name || !form.target) return toast("Name and target are required", "bad");
    el.remove();
    void runTask(`Create pool ${form.name}`, async () => {
      await api.postXml(`/proxy/${selectedConn}/pools`, poolXml(form));
    }, { done: `Pool ${form.name} created`, notify: toast })
      .then(() => { void refreshTreeVms(); renderConn(); });
  };
}

function openCreateVolume(pool: string) {
  if (!selectedConn) return;
  const el = openModal(
    `New volume in ${pool}`,
    `<div class="grid2">
      <label class="field"><span>Name</span><input id="vol-name"></label>
      <label class="field"><span>Size (GB)</span><input id="vol-size" type="number" value="10" min="1"></label>
      <label class="field"><span>Format</span><select id="vol-format"><option>qcow2</option><option>raw</option></select></label>
    </div>`,
    `<button class="btn primary" id="vol-create">Create</button>`,
  );
  $("#vol-create", el).onclick = async () => {
    const name = $("#vol-name", el).value;
    const size = +$("#vol-size", el).value;
    const format = ($("#vol-format", el) as HTMLSelectElement).value as "raw" | "qcow2";
    if (!name || !size) return toast("Name and size required", "bad");
    el.remove();
    void runTask(`Create volume ${name}`, async () => {
      await api.post(`/proxy/${selectedConn}/pools/${encodeURIComponent(pool)}/volumes`, {
        xml: volumeXml({ name, capacity_gb: size, format }),
      });
    }, { done: `Volume ${name} created`, notify: toast });
  };
}

// ---------------------------------------------------------------- uploads
//
// A staged transfer runs to completion on the server whether or not this page
// is open, so this list is a *view* of the server's state rather than a
// client-side task queue: it is refetched, and only the transfers section is
// redrawn. That is why navigating away mid-transfer is safe.

/// Uploads known to the server, oldest first.
let activeUploads: StagedUpload[] = [];
let uploadTimer: number | null = null;
/// Last phase seen per id, so a completion is announced exactly once.
const uploadSeen = new Map<string, string>();

function isLiveUpload(u: StagedUpload): boolean {
  return u.phase === "staged" || u.phase === "uploading" || u.phase === "verifying";
}

const PHASE_LABEL: Record<string, string> = {
  staged: "Ready",
  uploading: "To daemon",
  verifying: "Verifying",
  done: "Verified",
  failed: "Failed",
};

/// Start the poll timer. Separate from `pollUploads` so a poll can restart the
/// timer without recursing into another poll.
function startUploadPoll() {
  if (uploadTimer !== null) return;
  uploadTimer = window.setInterval(() => void pollUploads(), 700);
}

function stopUploadPoll() {
  if (uploadTimer === null) return;
  window.clearInterval(uploadTimer);
  uploadTimer = null;
}

async function pollUploads() {
  if (!selectedConn) return stopUploadPoll();
  let jobs: StagedUpload[];
  try {
    jobs = await api.uploads();
  } catch {
    return; // transient; the running timer retries
  }
  activeUploads = jobs;

  for (const j of jobs) {
    const before = uploadSeen.get(j.id);
    uploadSeen.set(j.id, j.phase);
    if (before === j.phase) continue;
    if (j.phase === "done") {
      // The daemon's copy is verified and the staged file is gone.
      toast(`${j.name} stored on the daemon`);
      if (selectedPool === j.pool) void renderPoolPage();
    } else if (j.phase === "failed") {
      toast(`${j.name}: ${j.error ?? "transfer failed"}`, "bad");
    }
  }
  if (jobs.some(isLiveUpload)) startUploadPoll();
  else stopUploadPoll();
  refreshUploadBars();
}

/// One card per transfer, reusing the pool page's metric card so no new
/// styling is introduced for this.
function uploadsSection(pool: string): string {
  const jobs = activeUploads.filter((j) => j.pool === pool);
  if (!jobs.length) return "";
  return `
    <div class="section-title fw-cat" style="margin-top:18px">Transfers</div>
    ${jobs.map((j) => {
      const live = isLiveUpload(j);
      const done = j.phase === "done";
      const failed = j.phase === "failed";
      const p = j.size > 0 ? Math.min(100, (j.sent / j.size) * 100) : 0;
      // A finished transfer reads as full even if the byte count ran short.
      const width = live ? p : 100;
      // `speed_bps` is bytes/s; fmtRate speaks KiB/s.
      const speed = j.speed_bps > 0 ? fmtRate(j.speed_bps / 1024) : "";
      // The daemon times its receive and its write separately, so when the two
      // disagree the slower one names the bottleneck. Only known at the end,
      // since that is when its response arrives.
      const daemonRates = j.daemon_recv_bps > 0 || j.daemon_write_bps > 0
        ? `daemon recv ${fmtRate(j.daemon_recv_bps / 1024)} · write ${fmtRate(j.daemon_write_bps / 1024)}`
        : "";
      return `<div class="metric" data-up-id="${esc(j.id)}">
        <div class="metric-head">
          <span class="metric-ic">${ic.upload()}</span>
          <b>${esc(j.name)}</b>
          <span class="metric-value">${esc(PHASE_LABEL[j.phase] ?? j.phase)}</span>
        </div>
        <div class="meter"><div class="meter-fill ${failed ? "hot" : ""}" style="width:${width.toFixed(1)}%"></div></div>
        <div class="metric-foot">
          <span>${fmtBytes(j.sent)} of ${fmtBytes(j.size)}</span>
          <span>${speed}</span>
          <span>${done && j.sha256 ? `sha256 ${esc(j.sha256.slice(0, 12))}…` : ""}</span>
          ${j.phase === "staged" ? `<button class="btn" data-up-push="${esc(j.id)}">Send to daemon</button>` : ""}
          <button class="btn icon" data-up-${live ? "cancel" : "clear"}="${esc(j.id)}"
            title="${live ? "Cancel and discard the staged copy" : "Dismiss"}">${ic.close()}</button>
        </div>
        ${done && daemonRates ? `<p class="hint">${daemonRates}</p>` : ""}
        ${failed ? `<p class="hint">${esc(j.error ?? "transfer failed")}</p>` : ""}
      </div>`;
    }).join("")}`;
}

/// Redraw only the transfers section, so polling never rebuilds the volume
/// table underneath whoever is using it.
function refreshUploadBars() {
  const host = document.getElementById("pool-uploads");
  if (!host || !selectedPool) return;
  setHtml(host, uploadsSection(selectedPool));
  bindUploads();
}

function bindUploads() {
  const host = document.getElementById("pool-uploads");
  if (!host) return;
  const drop = async (id: string, keep: boolean) => {
    try {
      await api.discardUpload(id);
    } catch (e) {
      return toast((e as Error).message, "bad");
    }
    uploadSeen.delete(id);
    if (keep) uploadSeen.set(id, "gone");
    await pollUploads();
  };
  for (const b of host.querySelectorAll<HTMLElement>("[data-up-cancel]")) {
    b.onclick = () => void drop(b.dataset.upCancel!, true);
  }
  for (const b of host.querySelectorAll<HTMLElement>("[data-up-clear]")) {
    b.onclick = () => void drop(b.dataset.upClear!, true);
  }
  // A job that came back from a previous run is already staged on disk, so the
  // bytes do not need to be sent again: one click restarts the daemon leg.
  for (const b of host.querySelectorAll<HTMLElement>("[data-up-push]")) {
    b.onclick = async () => {
      const id = b.dataset.upPush!;
      b.setAttribute("disabled", "true");
      try {
        await api.pushUpload(id);
      } catch (e) {
        b.removeAttribute("disabled");
        return toast((e as Error).message, "bad");
      }
      await pollUploads();
    };
  }
}

// ------------------------------------------------------------ URL fetches

interface FetchJob {
  id: string; pool: string; name: string; url: string;
  phase: "downloading" | "verifying" | "writing" | "done" | "failed" | "cancelled";
  received: number; total: number | null; speed_bps: number;
  sha256: string | null; error: string | null;
}
/// Last known fetches, per server. The daemon owns the jobs; this is only
/// what the pool page last saw.
const fetchJobs = new Map<string, FetchJob[]>();
const fetchSeen = new Map<string, string>();
let fetchTimer: number | null = null;
const FETCH_LABEL: Record<string, string> = {
  downloading: "Downloading", verifying: "Verifying", writing: "Writing",
  done: "Stored", failed: "Failed", cancelled: "Cancelled",
};
const liveFetch = (j: FetchJob) => j.phase === "downloading" || j.phase === "verifying" || j.phase === "writing";

/// Ask the daemon to download an ISO into `pool` itself. The bytes never pass
/// through this server or the browser, and the job survives closing the tab.
function openFetch(pool: string) {
  if (!selectedConn) return;
  const conn = selectedConn;
  const el = openModal(
    `Download ISO into ${esc(pool)}`,
    `<label class="field"><span>URL</span>
       <input id="fetch-url" placeholder="https://…/image.iso" spellcheck="false"></label>
     <label class="field"><span>Volume Name</span>
       <input id="fetch-name" placeholder="Leave blank to use the file name"></label>
     <label class="field"><span>SHA-256 (optional)</span>
       <input id="fetch-sha" placeholder="Checked before the image is stored" spellcheck="false"></label>
     <p class="hint">The server downloads the image itself; you can close this page while it runs.</p>`,
    `<button class="btn primary" id="fetch-go">Download</button>`,
  );
  const url = $("#fetch-url", el) as HTMLInputElement;
  const name = $("#fetch-name", el) as HTMLInputElement;
  // Suggest the URL's file name, but never overwrite one the user typed.
  let autoName = "";
  url.addEventListener("input", () => {
    let file = "";
    try { file = decodeURIComponent(new URL(url.value.trim()).pathname.split("/").pop() ?? ""); } catch { /* not a URL yet */ }
    const next = autoVolumeName(name.value, autoName, file);
    if (next === null) return;
    name.value = next;
    autoName = next;
  });
  const go = $("#fetch-go", el) as HTMLButtonElement;
  go.onclick = async () => {
    go.disabled = true;
    try {
      await api.post(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/fetch`, {
        url: url.value.trim(),
        name: name.value.trim() || undefined,
        sha256: ($("#fetch-sha", el) as HTMLInputElement).value.trim() || undefined,
      });
    } catch (e) {
      go.disabled = false;
      return toast((e as Error).message, "bad");
    }
    el.remove();
    toast("Download started on the server");
    await pollFetches(conn);
  };
  url.focus();
}

async function pollFetches(conn: string) {
  let jobs: FetchJob[];
  try {
    jobs = await api.get<FetchJob[]>(`/proxy/${conn}/fetches`);
  } catch {
    return; // transient, or the daemon is down: the next poll retries
  }
  fetchJobs.set(conn, jobs);
  for (const j of jobs) {
    const before = fetchSeen.get(j.id);
    fetchSeen.set(j.id, j.phase);
    if (before === undefined || before === j.phase) continue;
    if (j.phase === "done") {
      toast(`${j.name} downloaded and stored`);
      void api.post("/activity", { label: `Download ${j.name} into pool ${j.pool}`, status: "done", message: j.url }).catch(() => {});
      if (selectedPool === j.pool && selectedConn === conn) void renderPoolPage();
    } else if (j.phase === "failed") {
      toast(`${j.name}: ${j.error ?? "download failed"}`, "bad");
      void api.post("/activity", { label: `Download ${j.name} into pool ${j.pool}`, status: "failed", message: `${j.error ?? ""} (${j.url})` }).catch(() => {});
    }
  }
  if (jobs.some(liveFetch)) {
    if (fetchTimer === null) fetchTimer = window.setInterval(() => void pollFetches(conn), 1000);
  } else if (fetchTimer !== null) {
    window.clearInterval(fetchTimer);
    fetchTimer = null;
  }
  const host = document.getElementById("pool-fetches");
  if (host && selectedPool && selectedConn === conn) {
    setHtml(host, fetchesSection(conn, selectedPool));
    bindFetches(conn);
  }
}

function fetchesSection(conn: string, pool: string): string {
  const jobs = (fetchJobs.get(conn) ?? []).filter((j) => j.pool === pool);
  if (!jobs.length) return "";
  return `
    <div class="section-title fw-cat" style="margin-top:18px">Downloads</div>
    ${jobs.map((j) => {
      const live = liveFetch(j);
      const pctDone = j.total ? Math.min(100, (j.received / j.total) * 100) : 0;
      const width = live ? (j.total ? pctDone : 0) : 100;
      return `<div class="metric" data-fetch-id="${esc(j.id)}">
        <div class="metric-head">
          <span class="metric-ic">${ic.download()}</span>
          <b>${esc(j.name)}</b>
          <span class="metric-value">${esc(FETCH_LABEL[j.phase] ?? j.phase)}</span>
        </div>
        <div class="meter"><div class="meter-fill ${j.phase === "failed" ? "hot" : ""}" style="width:${width.toFixed(1)}%"></div></div>
        <div class="metric-foot">
          <span>${fmtBytes(j.received)}${j.total ? ` of ${fmtBytes(j.total)}` : ""}</span>
          <span>${live && j.speed_bps > 0 ? fmtRate(j.speed_bps / 1024) : ""}</span>
          <span>${j.phase === "done" && j.sha256 ? `sha256 ${esc(j.sha256.slice(0, 12))}…` : ""}</span>
          <button class="btn icon" data-fetch-${live ? "cancel" : "clear"}="${esc(j.id)}"
            title="${live ? "Cancel the download" : "Dismiss"}">${ic.close()}</button>
        </div>
        <p class="hint" title="${esc(j.url)}">${esc(j.phase === "failed" ? j.error ?? "download failed" : j.url)}</p>
      </div>`;
    }).join("")}`;
}

function bindFetches(conn: string) {
  const host = document.getElementById("pool-fetches");
  if (!host) return;
  for (const b of host.querySelectorAll<HTMLElement>("[data-fetch-cancel], [data-fetch-clear]")) {
    b.onclick = async () => {
      const id = b.dataset.fetchCancel ?? b.dataset.fetchClear!;
      try {
        await api.del(`/proxy/${conn}/fetches/${id}`);
      } catch (e) {
        return toast((e as Error).message, "bad");
      }
      await pollFetches(conn);
    };
  }
}

function openUpload(pool: string) {
  if (!selectedConn) return;
  const conn = selectedConn;
  const el = openModal(
    `Upload ISO to ${pool}`,
    `<label class="field"><span>Volume Name</span>
       <input id="up-name" placeholder="Leave blank to use the file name"></label>
     <label class="field"><span>File</span><input id="up-file" type="file"></label>
     <div id="up-bar" hidden>
       <div class="meter"><div class="meter-fill" id="up-fill" style="width:0%"></div></div>
       <p class="hint" id="up-status"></p>
     </div>`,
    `<button class="btn" id="up-cancel" hidden>Cancel</button>
     <button class="btn primary" id="up-go">Upload</button>`,
  );

  const nameInput = $("#up-name", el) as HTMLInputElement;
  const fileInput = $("#up-file", el) as HTMLInputElement;
  const goBtn = $("#up-go", el) as HTMLButtonElement;
  const cancelBtn = $("#up-cancel", el) as HTMLButtonElement;
  const bar = $("#up-bar", el) as HTMLElement;
  const fill = $("#up-fill", el) as HTMLElement;
  const status = $("#up-status", el) as HTMLElement;
  // The name we filled in from a file, so we can tell an untouched field from
  // one the user typed in: re-picking a file must not clobber a custom name.
  let autoName = "";
  fileInput.addEventListener("change", () => {
    const file = fileInput.files?.[0];
    if (!file) return;
    const next = autoVolumeName(nameInput.value, autoName, file.name);
    if (next === null) return; // the user typed a name: leave it alone
    nameInput.value = next;
    autoName = next;
  });

  goBtn.onclick = async () => {
    const file = fileInput.files?.[0];
    if (!file) return toast("Pick a file to upload", "bad");
    // Blank means "name it after the file", so the field is optional.
    const name = nameInput.value.trim() || volumeNameFromFile(file.name);
    if (!name) return toast("Name is required", "bad");

    // Phase 1: browser -> this server. The file has to land here first, because
    // holding the bytes is what lets the server verify the daemon afterwards.
    goBtn.disabled = true;
    nameInput.disabled = true;
    bar.hidden = false;
    cancelBtn.hidden = false;
    status.textContent = "Preparing…";

    // Timed from the moment the body starts leaving the browser, so the rate
    // reflects this hop only (the browser -> server leg).
    const startedAt = Date.now();
    const { promise, cancel } = uploadWithProgress(
      `/uploads?conn=${encodeURIComponent(conn)}&pool=${encodeURIComponent(pool)}&name=${encodeURIComponent(name)}`,
      file,
      (sent, total) => {
        const p = total > 0 ? (sent / total) * 100 : 0;
        fill.style.width = `${p.toFixed(1)}%`;
        // Null until it is worth showing, rather than a misleading 0 KiB/s.
        const rate = avgRateKib(sent, startedAt, Date.now());
        const speed = rate === null ? "" : ` \u2014 ${fmtRate(rate)}`;
        status.textContent = `Uploading to server \u2014 ${fmtBytes(sent)} of ${fmtBytes(total)} (${p.toFixed(0)}%)${speed}`;
      },
    );
    cancelBtn.onclick = () => cancel();

    let staged: StagedUpload;
    try {
      staged = await promise;
    } catch (e) {
      // Keep the dialog open so the user can retry without re-picking the file.
      goBtn.disabled = false;
      nameInput.disabled = false;
      cancelBtn.hidden = true;
      status.textContent = `Failed: ${(e as Error).message}`;
      return toast(`Upload failed: ${(e as Error).message}`, "bad");
    }

    // Phase 2 runs on the server, so the dialog can close. Progress moves to
    // the pool page, where it survives navigating away and back.
    el.remove();
    toast(`Uploaded ${name} to the server — sending to daemon`);
    startUploadPoll();
    try {
      await api.pushUpload(staged.id);
    } catch (e) {
      toast(`Could not start the daemon transfer: ${(e as Error).message}`, "bad");
    }
    await pollUploads();
  };
}

// ------------------------------------------------------------------ profile

/// Avatar markup shared by the sidebar button and the profile card.
function avatarHtml(): string {
  return user?.avatar
    ? `<img src="/avatars/${encodeURIComponent(user.avatar)}" alt="">`
    : (user?.nickname[0] ?? "u").toUpperCase();
}

/// Repaint every place the identity is shown, after any profile change.
function renderIdentity() {
  renderProfileBtn();
  const host = $(".topbar .host");
  if (host && user) {
  host.textContent = `web${authDisabled ? " · local" : ` · ${user.nickname}`}`;
  }
}

/// One profile card: avatar (click to crop), display name, username,
/// password and API keys. Replaces the old popover + separate modal split.
function openProfile() {
  if (!user) return;
  const el = openModal(
    "Profile",
    `<div class="profile-id">
       <button class="pop-avatar" id="pf-avatar-btn" title="Change picture">
         <div class="avatar">${avatarHtml()}</div>
         <span class="camera-badge">${ic.camera()}</span>
       </button>
       <div class="who">
         <b id="pf-who-name">${esc(user.nickname)}</b>
         <span id="pf-who-user">@${esc(user.username)} · ${user.admin ? "administrator" : "user"}</span>
       </div>
     </div>

      <div class="grid2">
       <label class="field"><span>Display Name</span>
          <input id="pf-nick" value="${esc(user.nickname)}"></label>
       <label class="field"><span>Username (used to sign in)</span>
         <input id="pf-user" value="${esc(user.username)}" autocomplete="username"></label>
     </div>

     <div class="section-title">Password</div>
     <div class="grid2">
       <label class="field"><span>Current Password</span>
         <input id="pf-pass" type="password" autocomplete="current-password"></label>
       <label class="field"><span>New password</span>
         <input id="pf-pass2" type="password" autocomplete="new-password" placeholder="min 8 characters"></label>
     </div>
     <div class="actions"><button class="btn" id="pf-pass-save">${ic.lock()} Change password</button></div>

     <div class="section-title">API Keys</div>
     <div class="key-list" id="pf-keys"></div>
     <div class="actions"><button class="btn" id="pf-newkey">${ic.key()} New API key</button></div>

     <div id="pf-limits"></div>`,
    // Save sits next to Cancel and only appears once something differs, so the
    // footer never offers a no-op. Cancel/Close is the same button relabelled:
    // with pending edits, closing discards them.
    `<button class="btn" id="pf-close">Close</button>
     <button class="btn primary hidden" id="pf-save">Save Changes</button>`,
  );
  $("#pf-close", el).onclick = () => el.remove();
  $("#pf-avatar-btn", el).onclick = () => void changeAvatar(el);
  void loadProfileLimits(el);

  // Show Save only when the form actually differs from the stored identity.
  const nickInput = $("#pf-nick", el) as HTMLInputElement;
  const userInput = $("#pf-user", el) as HTMLInputElement;
  const saveBtn = $("#pf-save", el) as HTMLButtonElement;
  const closeBtn = $("#pf-close", el) as HTMLButtonElement;
  const syncDirty = () => {
    const dirty = nickInput.value.trim() !== (user?.nickname ?? "")
      || userInput.value.trim() !== (user?.username ?? "");
    saveBtn.classList.toggle("hidden", !dirty);
    closeBtn.textContent = dirty ? "Cancel" : "Close";
  };
  nickInput.oninput = syncDirty;
  userInput.oninput = syncDirty;

  // Display name and username save together: both live on PUT /me, so one
  // button means one round trip and no half-applied state.
  saveBtn.onclick = async () => {
    const nickname = nickInput.value.trim();
    const username = userInput.value.trim();
    if (!nickname) return toast("Display name is required", "bad");
    if (!username) return toast("Username is required", "bad");
    const before = user?.username;
    try {
      user = await api.put<User>("/me", { nickname, username });
      toast(username === before ? "Saved" : "Saved — sign in with the new username next time");
      renderIdentity();
      refreshProfileCard(el);
      // Saved values are the new baseline, so the button retires itself.
      syncDirty();
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };

  $("#pf-pass-save", el).onclick = async () => {
    const current = $("#pf-pass", el).value;
    const next = $("#pf-pass2", el).value;
    if (!current || !next) return toast("Enter current and new password", "bad");
    try {
      await api.post("/me/password", { current, new: next });
      toast("Password changed");
      ($("#pf-pass", el) as HTMLInputElement).value = "";
      ($("#pf-pass2", el) as HTMLInputElement).value = "";
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };

  const refreshKeys = async () => {
    const keys = await api.get<import("./api").ApiKey[]>("/me/keys");
    const box = $("#pf-keys", el);
setHtml(box, keys.map((k) => `<div class="task-row"><span class="task-dot done"></span><span class="task-msg">${esc(k.name)} · ${esc(k.prefix)}…</span><button class="btn danger icon" data-kid="${k.id}">${ic.trash()}</button></div>`).join("") || `<div class="empty">No API keys.</div>`);
    box.querySelectorAll("[data-kid]").forEach((b: Element) => {
      (b as HTMLElement).onclick = async () => {
        await api.del(`/me/keys/${(b as HTMLElement).dataset.kid}`);
        refreshKeys();
      };
    });
  };
  refreshKeys();
  $("#pf-newkey", el).onclick = async () => {
    const name = await askText(
      "New API key", "Key name (blank to generate one)", "", "leave blank for a random name");
    // askText returns null when cancelled and "" when accepted while empty, so
    // only null aborts; empty means "let the server name it".
    if (name === null) return;
    try {
      const k = await api.post<import("./api").ApiKey>("/me/keys", { name: name.trim() });
      refreshKeys();
      // Shown once and never again, so it gets a dismissable dialog rather
      // than a toast on a timer.
      showSecret(`API key — ${k.name}`, k.key ?? "");
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };
}

/// Repaint the card's identity header from the current user.
/// The profile's Resource Limits section: what the user may use, and what
/// they do. Each limit is its own meter. Per-daemon limits are shown for each
/// daemon; pooled limits once, against the total across daemons.
async function loadProfileLimits(el: Element) {
  const box = $("#pf-limits", el);
  if (!box) return;
  type Used = { vms: number; vcpus: number; memory_mb: number; storage_bytes: number; networks: number };
  let u: {
    admin: boolean;
    quota: import("./api").Quota;
    total: Used;
    daemons: (Used & { conn: string; name: string; online: boolean })[];
  };
  try {
    u = await api.get<typeof u>("/me/usage");
  } catch {
    return; // the rest of the profile is still usable
  }
  if (!box.isConnected) return;
  const title = `<div class="section-title">Resource Limits</div>`;
  if (u.admin) {
    setHtml(box, `${title}<p class="hint">Administrators have no resource limits.</p>`);
    return;
  }
  const q = u.quota ?? { shared: false, default: {}, daemons: {} };
  const any = (l: import("./api").Limit) => [l.vms, l.vcpus, l.memory_mb, l.storage_gb, l.networks].some((x) => x != null);
  if (!any(q.default) && !Object.values(q.daemons ?? {}).some(any)) {
    setHtml(box, `${title}<p class="hint">You have no resource limits.</p>`);
    return;
  }
  const block = (name: string, used: Used, cap: import("./api").Limit, note = "") => `
    <div class="pf-limit">
      <div class="pf-limit-head"><b>${esc(name)}</b>${note ? `<span class="dim-note">${esc(note)}</span>` : ""}</div>
      ${limitSpecs(used, cap)}
    </div>`;
  let html = "";
  if (q.shared) html += block("All servers", u.total, q.default, "shared across servers");
  for (const d of u.daemons) {
    const own = q.daemons?.[d.conn];
    // Pooled: a daemon appears only where it has its own extra cap.
    if (q.shared && !own) continue;
    if (!d.online) {
      html += `<div class="pf-limit"><div class="pf-limit-head"><b>${esc(d.name)}</b><span class="dim-note">offline</span></div></div>`;
      continue;
    }
    html += block(d.name, d, own ?? q.default, own ? "server-specific limit" : "");
  }
  setHtml(box, `${title}${html}`);
}

function refreshProfileCard(el: Element) {
  if (!user) return;
  const name = $("#pf-who-name", el);
  if (name) name.textContent = user.nickname;
  const who = $("#pf-who-user", el);
  if (who) who.textContent = `@${user.username} · ${user.admin ? "administrator" : "user"}`;
  const av = $("#pf-avatar-btn .avatar", el);
  if (av) setHtml(av, avatarHtml());
}

/// Pick a file, crop it square, and upload it as the avatar.
async function changeAvatar(card?: Element) {
  if (!user) return;
  const input = document.createElement("input");
  input.type = "file";
  input.accept = "image/png,image/jpeg,image/webp";
  input.onchange = async () => {
    const file = input.files?.[0];
    if (!file) return;
    const blob = await openCropDialog(file);
    if (!blob) return;
    try {
      user = await api.postRaw<User>("/me/avatar?ext=png", blob, "image/png");
      toast("Avatar updated");
      renderIdentity();
      if (card) refreshProfileCard(card);
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };
  input.click();
}

// ---------------------------------------------------------------- tasks
// ---------------------------------------------------------------- polling

function startPolling() {
  if (pollTimer) window.clearInterval(pollTimer);
  pollTimer = window.setInterval(() => {
    if (!user) return;
    void refreshHealth();
    // The admin page reads on demand; re-reading every daemon's resources
    // each second would be the costliest poll in the app for no gain.
    if (adminSection) return;
    // A down server's views show the offline panel; polling its daemon would
    // only fail again and paint the raw error over it.
    if (isOffline(selectedConn)) return;
    // Keep the sidebar's guest states live regardless of the open pane.
    void refreshTreeVms();
    if (!selectedConn && !selectedVm && dcTab === "dashboard") {
      loadDcDashboard();
    } else if (selectedConn && !selectedVm) {
      // Resource pages poll their own data; only repaint when they are the
      // view actually on screen.
      if (selectedNet) renderNetPage();
      else if (selectedPool) renderPoolPage();
      else if (connTab === "dashboard") loadDashboard();
      else if (connTab === "vms") loadVms();
      else if (connTab === "networks") loadNetworks();
      else if (connTab === "storage") loadPools();
      else loadFilters();
    } else if (selectedConn && selectedVm && vmTab === "summary") {
      renderVmBody(selectedVm);
    }
  }, 1000);
}

// ---------------------------------------------------------------- helpers

function cssId(s: string): string {
  return s.replace(/[^a-zA-Z0-9]/g, "_");
}

boot();
