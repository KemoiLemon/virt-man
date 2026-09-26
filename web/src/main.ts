import { openCropDialog } from "./crop";
import { api } from "./api";
import type { Connection, CreateConnectionResult, DomainSummary, FirewallForward, FirewallPolicy, FirewallRule, NetworkSummary, Permission, PoolSummary, RoleDef, User, VolumeSummary } from "./api";
import { attachConsole, detachConsole } from "./console";
import { closeTayne, openTayne, tayneOpen, type TayneCommand } from "./tayne";
import DOMPurify from "dompurify";
import { ic } from "./icons";
import { HW_CATALOG, deviceSummary, deviceXml, devicesFromXml, hwType, validateDevice, withDevices } from "./hardware";
import type { HwDevice } from "./hardware";
import { enhanceSelects } from "./listbox";
import { addDiskSample, addHostSample, addSample, fmtBytes, pct, sparkline } from "./stats";
import type { DomainStats, HostStats } from "./stats";
import { allTasks, clearTask, onTasks, runTask } from "./task";
import { domainXml, esc, networkFormFromXml, networkUuid, networkXml, poolXml, volumeXml } from "./xml";
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

/// Single HTML injection point. Every template's dynamic parts are escaped via
/// esc() at the call site; DOMPurify is the sanitizing backstop the XSS rule
/// requires.
function setHtml(el: Element, html: string) {
  // pi-lens-ignore: no-inner-html, no-inner-html-js
  el.innerHTML = DOMPurify.sanitize(html);
  // Every render path funnels through here, so this is the one place that has
  // to swap native <select> elements for the themed listbox.
  enhanceSelects(el);
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
let renderedPools: string[] = [];
let selectedVm: string | null = null;
let vmTab: "summary" | "hardware" | "options" | "firewall" | "console" | "xml" = "summary";
let dcTab: "dashboard" | "servers" | "guests" = "dashboard";
/// Which section of the admin panel modal is showing.
let adminTab: "users" | "roles" | "keys" = "users";
/// Scope vocabulary and roles, fetched with the admin panel. The backend is
/// the source of truth for scope names, so the editor can never offer one
/// that nothing enforces.
let adminScopes: string[] = [];
let adminRoles: RoleDef[] = [];
let pollTimer: number | null = null;
/// Retries for a daemon saved while unreachable: re-fetches its cert every
/// second until it is up, then pins it.
let pendingPoll: number | null = null;
/// VMs per connection, so the sidebar tree can nest guests under their server
/// without every render refetching.
const treeVms: Record<string, DomainSummary[]> = {};
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
  const add = (id: string, label: string, hint: string | undefined, run: () => void) =>
    cmds.push({ id, label, hint, run });

  add("nav-dc", "Go to Datacenter", "Servers and guests", () => {
    leaveCurrentView();
    selectedConn = null;
    selectedVm = null;
    dcTab = "dashboard";
    renderTree();
    renderDc();
  });

  for (const c of connections) {
    add(`nav-server-${c.id}`, `Server — ${c.name}`, c.url, () => {
      leaveCurrentView();
      selectedConn = c.id;
      selectedVm = null;
      connTab = "dashboard";
      renderTree();
      void renderConn();
    });
    for (const v of treeVms[c.id] ?? []) {
      add(`nav-vm-${c.id}-${v.name}`, `VM — ${v.name}`, c.name, () => {
        leaveCurrentView();
        selectedConn = c.id;
        selectedVm = v.name;
        vmTab = "summary";
        renderTree();
        void renderVm();
      });
    }
  }

  // Actions on the selected guest, when one is open.
  if (selectedConn && selectedVm) {
    const conn = selectedConn;
    const vm = selectedVm;
    const power = (id: string, label: string, path: string) =>
      add(id, `${label} — ${vm}`, "Selected VM", async () => {
        try {
          await act(`/proxy/${conn}/domains/${encodeURIComponent(vm)}${path}`, "POST");
        } catch {
          return; // act already toasts the daemon's error
        }
        void renderVm();
      });
    power("vm-start", "Start", "/start");
    power("vm-shutdown", "Shutdown", "/shutdown");
    power("vm-reboot", "Reboot", "/reboot");
    power("vm-destroy", "Force stop", "/destroy");
    add("vm-console", `Open console — ${vm}`, "Selected VM", () => {
      leaveCurrentView();
      vmTab = "console";
      renderTree();
      void renderVm();
    });
    add("vm-hwedit", `Edit hardware — ${vm}`, "Selected VM", () => {
      void openHardwareEditor({ vm });
    });
    add("vm-rename", `Rename — ${vm}`, "Selected VM", () => {
      void renameVm(vm);
    });
    add("vm-delete", `Delete — ${vm}`, "Selected VM", () => {
      void deleteVm(vm);
    });
  }

  if (selectedConn && !selectedVm) {
    add("create-vm", "Create VM", "On the selected server", () => openCreateVm());
  }
  add("add-server", "Add server", "Connect to a daemon", () => openConnectionDialog());
  add(
    "toggle-theme",
    "Toggle theme",
    document.documentElement.getAttribute("data-theme") === "dark" ? "dark → light" : "light → dark",
    toggleTheme,
  );
  if (canAdmin()) {
    add("admin-panel", "Admin panel", "Users, permissions, API keys", () => openAdmin());
  }
  if (!authDisabled) add("logout", "Sign out", "End this session", () => void logout());

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

function toast(msg: string, kind: "ok" | "bad" = "ok") {
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  el.textContent = msg;
  document.body.appendChild(el);
  requestAnimationFrame(() => el.classList.add("show"));
  setTimeout(() => {
    el.classList.remove("show");
    setTimeout(() => el.remove(), 200);
  }, 4000);
}

// ---------------------------------------------------------------- boot

async function boot() {
  initTheme();
  const status = await api.get<{ needs_setup: boolean; auth_disabled?: boolean }>("/auth/status");
  if (status.auth_disabled) {
    authDisabled = true;
    user = { id: "local", username: "local", role: "admin", nickname: "local", avatar: null, scopes: {} };
    await loadConnections();
    renderApp();
    return;
  }
  if (status.needs_setup) return renderSetup();
  try {
    const me = await api.get<User>("/auth/me");
    user = me;
    await loadConnections();
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
      <span class="brand">virt-<span>man</span></span>
      <span class="host">web${authDisabled ? " · local" : ` · ${esc(user.nickname)}`}</span>
      <span class="topbar-quick">
        <button class="btn icon" id="theme-btn" title="Toggle theme">${ic.theme()}</button>
        ${canAdmin() ? `<button class="btn icon" id="admin-btn" title="Admin panel">${ic.admin()}</button>` : ""}
        ${authDisabled ? "" : `<button class="btn icon" id="logout-btn" title="Sign out">${ic.logout()}</button>`}
      </span>
    </div>
    <div class="layout">
      <div class="pane list-pane">
        <div class="restree" id="tree"></div>
        <button class="tree-add" id="add-conn-row">${ic.plus(14)} Add Server</button>
        ${authDisabled ? "" : `<div class="side-profile" id="profile-btn"></div>`}
      </div>
      <div class="pane">
        <div class="detail" id="content"></div>
      </div>
    </div>
    <div class="tasks hidden" id="tasks">
      <div class="tasks-head"><span>Task Log</span><button class="btn icon" id="tasks-close">${ic.close()}</button></div>
      <div class="tasks-body" id="tasks-body"></div>
    </div>`);

  $("#theme-btn").onclick = toggleTheme;
  if ($("#logout-btn")) $("#logout-btn").onclick = logout;
  if ($("#admin-btn")) $("#admin-btn").onclick = openAdmin;
  $("#add-conn-row").onclick = () => openConnectionDialog();
  $("#tasks-close").onclick = () => $("#tasks").classList.add("hidden");

  onTasks(renderTasks);
  renderTree();
  renderProfileBtn();
  if (!selectedConn) renderDc();
  // Populate the auto-expanded tree immediately rather than waiting for the
  // first poll tick or a server click.
  void refreshTreeVms();
  startPolling();
}

/// Whether to offer the admin panel. Mirrors the backend's `permission()`
/// for the "users" scope: an explicit per-user override wins, otherwise the
/// role decides. Hiding the button is cosmetic — the API enforces the same
/// rule, so a non-admin poking at /users still gets 403.
function canAdmin(): boolean {
  if (!user) return false;
  const override = user.scopes?.users;
  if (override) return override === "write";
  return user.role === "admin";
}

// ---------------------------------------------------------------- tree

/// Nested resource tree: Datacenter root -> servers -> guests, mirroring
/// vm-man's sidebar. Guests come from the per-connection cache, so opening a
/// server reveals its VMs inline instead of only in the content pane.
function renderTree() {
  const tree = $("#tree");
  if (!tree) return;

  const serverRows = connections
    .map((c) => {
      const selected = c.id === selectedConn && !selectedVm;
      const vms = treeVms[c.id] ?? [];
      const running = vms.filter((v) => v.active).length;
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
        <span class="tree-label">${esc(v.name)}</span>
        <span class="tree-state ${v.active ? "on" : ""}">${esc(capLabel(v.state))}</span>
      </button>`,
              )
              .join("")
          : "";
      return `
      <button class="tree-row tree-group ${selected ? "sel" : ""}" data-conn="${c.id}">
        <span class="tree-ic ${running > 0 ? "on" : ""}">${ic.server()}</span>
        <span class="tree-label">${esc(c.name)}</span>
        <span class="tree-state">${vms.length ? `${running}/${vms.length}` : ""}</span>
      </button>${guestRows}`;
    })
    .join("");

  setHtml(tree, `
    <button class="tree-row tree-root ${selectedConn === null ? "sel" : ""}" id="dc-row">
      <span class="tree-ic">${ic.datacenter()}</span>
      <span class="tree-label">Datacenter</span>
    </button>
    ${serverRows || `<div class="tree-empty">No servers yet — add one below.</div>`}`);

  $("#dc-row").onclick = () => {
    leaveCurrentView();
    selectedConn = null;
    selectedVm = null;
    dcTab = "dashboard";
    renderTree();
    renderDc();
  };
  tree.querySelectorAll("[data-conn]").forEach((row: Element) => {
    (row as HTMLElement).onclick = () => {
      leaveCurrentView();
      selectedConn = (row as HTMLElement).dataset.conn!;
      selectedVm = null;
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
      selectedVm = el.dataset.vm!;
      vmTab = "summary";
      renderTree();
      void renderVm();
    };
  });
}

/// Refresh the guest list backing the tree for the selected server.
async function refreshTreeVms() {
  // Every server's guests, not just the selected one: the tree auto-expands,
  // so an unselected server still has to show its VMs.
  const results = await Promise.all(
    connections.map(async (c) => {
      try {
        const vms = await api.get<DomainSummary[]>(`/proxy/${c.id}/domains`);
        return [c.id, vms] as const;
      } catch {
        // Unreachable daemon: keep whatever we last knew rather than
        // collapsing its branch. The content pane surfaces the error.
        return [c.id, treeVms[c.id] ?? []] as const;
      }
    }),
  );

  let changed = false;
  for (const [id, vms] of results) {
    if (JSON.stringify(treeVms[id]) !== JSON.stringify(vms)) {
      treeVms[id] = vms;
      changed = true;
    }
  }
  // Only re-render when something actually moved, so the tree does not
  // flicker or fight the user's clicks on every poll.
  if (changed) {
    renderTree();
    // The Datacenter view lists the same guests, so keep it in step; without
    // this its Guests table stays empty until another action redraws it.
    if (!selectedConn && !selectedVm) renderDc();
  }
}

function renderProfileBtn() {
  const el = $("#profile-btn");
  if (!el || !user) return;
  el.onclick = openProfile;
  setHtml(el, `
    <div class="avatar">${avatarHtml()}</div>
    <div class="who"><b>${esc(user.nickname)}</b><span>${user.role}</span></div>`);
}

// ---------------------------------------------------------------- datacenter

function renderDc() {
  const content = $("#content");
  // Real tabbar; the Users tab is hidden when the local app disables auth.
  // Actions live on the tab bar itself, right-aligned after the tabs.
  const tabs = `<div class="tabbar">
      <button class="tab ${dcTab === "dashboard" ? "active" : ""}" data-dctab="dashboard">${ic.datacenter(14)} Dashboard</button>
      <button class="tab ${dcTab === "servers" ? "active" : ""}" data-dctab="servers">${ic.server(14)} Servers</button>
      <button class="tab ${dcTab === "guests" ? "active" : ""}" data-dctab="guests">${ic.vms(14)} Guests</button>
      ${dcTab === "servers"
        ? `<div class="tabbar-actions"><button class="btn primary" id="dc-add">${ic.plus(14)} Add Server</button></div>`
        : ""}
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
      <div class="storage-card ${vm.active ? "ok" : ""}" data-open data-dc-vm="${esc(vm.name)}" data-dc-vm-conn="${conn.id}">
        <div class="storage-card-head">
          <span class="storage-card-icon">${ic.vm()}</span>
          <span class="storage-card-title">${esc(vm.name)}</span>
          <span class="badge">${esc(conn.name)}</span>
          <span class="badge ${vm.active ? "on" : ""}">${vm.active ? "Active" : "Inactive"}</span>
        </div>
        <div class="specs">
          <div class="spec"><b>vCPUs</b><span>${vm.vcpus || "—"}</span></div>
          <div class="spec"><b>Memory</b><span>${vm.memory_mb ? fmtBytes(vm.memory_mb * 1024) : "—"}</span></div>
        </div>
        <div class="storage-card-actions net-actions">
          <button class="btn icon ${vm.active ? "on" : ""}" data-dc-toggle="${esc(vm.name)}" data-dc-toggle-conn="${conn.id}" data-dc-action="${vm.active ? "stop" : "start"}" title="${vm.active ? "Shutdown" : "Start"}">${vm.active ? ic.stop() : ic.start()}</button>
          <button class="btn icon" data-dc-console="${esc(vm.name)}" data-conn="${conn.id}" title="Console">${ic.vm()}</button>
          <button class="btn icon" data-dc-hwedit="${esc(vm.name)}" data-conn="${conn.id}" title="Edit hardware">${ic.edit()}</button>
        </div>
      </div>`).join("");

  const serverCards = connections.map((c) => `
      <div class="storage-card" data-open data-open-conn-row="${c.id}">
        <div class="storage-card-head">
          <span class="storage-card-icon">${ic.server()}</span>
          <span class="storage-card-title">${esc(c.name)}</span>
          <span class="badge">${esc(c.url)}</span>
        </div>
        <div class="specs">
          <div class="spec"><b>URL</b><span>${esc(c.url)}</span></div>
          <div class="spec"><b>Cert</b><span>${c.fingerprint ? esc(c.fingerprint) : "—"}</span></div>
        </div>
        <div class="storage-card-actions net-actions">
          <button class="btn icon" data-edit="${c.id}" title="Edit server">${ic.edit()}</button>
          <button class="btn danger icon" data-del="${c.id}" title="Remove server">${ic.trash()}</button>
        </div>
      </div>`).join("");

  let body: string;
  if (dcTab === "dashboard") {
    body = `<div id="dc-dash">loading…</div>`;
  } else if (dcTab === "servers") {
    body = `${cards}<div class="section-title">Servers</div>
       <div class="card-grid">
         ${serverCards}
         <button class="storage-card create-card" id="dc-server-create">
           <span class="storage-card-icon">${ic.plus()}</span>
           <span class="storage-card-title">Add Server</span>
         </button>
       </div>`;
  } else if (dcTab === "guests") {
    body = `${cards}<div class="section-title">Guests</div>
       <div class="card-grid">
         ${guestCards}
         <button class="storage-card create-card" id="dc-guest-create">
           <span class="storage-card-icon">${ic.plus()}</span>
           <span class="storage-card-title">Create Guest</span>
         </button>
       </div>`;
  } else {
    body = `<div id="users-area">loading…</div>`;
  }
setHtml(content, `<div class="dhead"><span class="dtitle">Datacenter</span></div>${tabs}${body}`);

  if (dcTab === "dashboard") void loadDcDashboard();

  $("#dc-add")?.addEventListener("click", () => openConnectionDialog());
  $("#dc-server-create")?.addEventListener("click", () => openConnectionDialog());
  $("#dc-guest-create")?.addEventListener("click", () => openCreateVm());
  content.querySelectorAll("[data-dctab]").forEach((b: Element) => {
    (b as HTMLElement).onclick = () => {
      dcTab = (b as HTMLElement).dataset.dctab as typeof dcTab;
      renderDc();
    };
  });
  content.querySelectorAll("[data-edit]").forEach((b: Element) => {
    (b as HTMLElement).onclick = (e) => {
      e.stopPropagation();
      openConnectionDialog((b as HTMLElement).dataset.edit!);
    };
  });
  content.querySelectorAll("[data-del]").forEach((b: Element) => {
    (b as HTMLElement).onclick = (e) => {
      e.stopPropagation();
      void deleteConnection((b as HTMLElement).dataset.del!);
    };
  });

  // Selecting a server card opens that server's view (buttons stop propagation).
  content.querySelectorAll("[data-open-conn-row]").forEach((r: Element) => {
    (r as HTMLElement).onclick = (e) => {
      if ((e.target as Element).closest?.("[data-edit], [data-del]")) return;
      selectedConn = (r as HTMLElement).dataset.openConnRow!;
      selectedVm = null;
      connTab = "dashboard";
      renderTree();
      void renderConn();
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

  const { cpu, mem, cpuNow, memNow } = addHostSample("datacenter", agg);
  const memUsedKib = agg.mem_total_kib - agg.mem_available_kib;
  const diskTotal = pools.reduce((a, { p }) => a + (p.capacity ?? 0), 0);
  const diskUsed = pools.reduce((a, { p }) => a + (p.allocation ?? 0), 0);
  const diskNow = diskTotal > 0 ? Math.max(0, Math.min(100, (diskUsed / diskTotal) * 100)) : 0;
  const disk = addDiskSample("datacenter", diskNow);
  const activeNets = nets.filter(({ n }) => n.active).length;
  const activePools = pools.filter(({ p }) => p.active).length;
  const byId = new Map(live.map(({ c, h, pools: ps }) => [c.id, { h, pools: ps }] as const));

  setHtml(body, `
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
          ${sparkline(cpu)}
          <div class="chart-axis"><span>-1m</span><span>now</span></div>
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
          ${sparkline(mem, "mem")}
          <div class="chart-axis"><span>-1m</span><span>now</span></div>
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
          ${sparkline(disk)}
          <div class="chart-axis"><span>-1m</span><span>now</span></div>
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
            return `<tr><td class="tname" style="cursor:pointer" data-dc-dash-conn="${c.id}">${esc(c.name)} <span class="state">Unreachable</span></td><td>—</td><td>—</td><td>—</td></tr>`;
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
            <td class="tname" style="cursor:pointer" data-dc-dash-conn="${c.id}">${esc(c.name)}</td>
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
          <tr><td>${esc(n.name)}</td><td>${esc(c.name)}</td><td>${esc(n.bridge)}</td>
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
        <div class="storage-card ${p.active ? "ok" : ""}">
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

// ----------------------------------------------------------- admin panel

/// Admin panel: users, permissions and API keys, plus room for the misc
/// server-wide settings that do not belong to any one daemon. Lives in a
/// modal rather than a Datacenter tab so it is reachable from any view.
function openAdmin() {
  if (!canAdmin()) return;
  const el = openModal(
    "Admin panel",
     `<div class="tabbar">
       <button class="tab" data-admintab="users">${ic.users(14)} Users</button>
       <button class="tab" data-admintab="roles">${ic.admin(14)} Roles</button>
       <button class="tab" data-admintab="keys">${ic.key(14)} API Keys</button>
     </div>
     <div id="admin-area">loading…</div>`,
    `<button class="btn" id="ad-close">Close</button>`,
  );
  (el.querySelector(".modal") as HTMLElement).classList.add("wide");
  $("#ad-close", el).onclick = () => el.remove();

  const paint = () => {
    el.querySelectorAll("[data-admintab]").forEach((b: Element) => {
      const tab = (b as HTMLElement).dataset.admintab as typeof adminTab;
      (b as HTMLElement).classList.toggle("active", tab === adminTab);
      (b as HTMLElement).onclick = () => {
        adminTab = tab;
        paint();
      };
    });
    void loadAdmin(el);
  };
  paint();
}

/// One <select> of permission choices. `current` of "" means inherit, which
/// the caller only offers where inheriting is meaningful.
function permSelect(attr: string, name: string, current: string, inheritLabel?: string): string {
  const opt = (v: string, label: string) =>
    `<option value="${v}" ${v === current ? "selected" : ""}>${esc(label)}</option>`;
  return `<select ${attr}="${esc(name)}">
    ${inheritLabel ? opt("", inheritLabel) : ""}
    ${opt("write", "write")}${opt("read", "read")}${opt("none", "none")}
  </select>`;
}

/// Short permission marker for the roles overview table.
function permMark(p: Permission | undefined): string {
  if (p === "write") return `<span class="perm w">W</span>`;
  if (p === "read") return `<span class="perm r">R</span>`;
  return `<span class="perm n">—</span>`;
}

async function loadAdmin(root: Element) {
  const area = $("#admin-area", root);
  if (!area) return;
  try {
    // Roles and the scope vocabulary drive both the users and roles tabs.
    const meta = await api.get<{ roles: RoleDef[]; scopes: string[] }>("/roles");
    adminRoles = meta.roles;
    adminScopes = meta.scopes;

    if (adminTab === "keys") {
      const [keys, users] = await Promise.all([
        api.get<import("./api").ApiKey[]>("/keys"),
        api.get<User[]>("/users"),
      ]);
      const owner = (id?: string) => users.find((u) => u.id === id)?.username ?? "—";
setHtml(area, `<table class="grid">
          <tr><th>Name</th><th>Owner</th><th>Prefix</th><th>Permissions</th><th class="col-actions"></th></tr>
          ${keys.map((k) => {
            const listed = Object.keys(k.scopes ?? {});
            return `<tr>
              <td>${esc(k.name)}</td>
              <td>${esc(owner(k.user_id))}</td>
              <td>${esc(k.prefix)}…</td>
              <td>${listed.length
                ? listed.map((s) => `<span class="perm-pair">${esc(s)}${permMark(k.scopes?.[s])}</span>`).join("")
                : `<span class="dim-note">Inherits Owner</span>`}</td>
              <td class="col-actions"><button class="btn danger icon" data-delkey="${k.id}" title="Revoke">${ic.trash()}</button></td>
            </tr>`;
          }).join("") || `<tr><td colspan="5" class="empty">No keys.</td></tr>`}
        </table>
        <div class="actions"><button class="btn primary" id="add-key">${ic.plus(14)} New API key</button></div>`);
      $("#add-key", area).onclick = () => openAddKey(users, root);
      area.querySelectorAll("[data-delkey]").forEach((b: Element) => {
        (b as HTMLElement).onclick = async () => {
          const id = (b as HTMLElement).dataset.delkey!;
          try { await api.del(`/keys/${id}`); toast("Key revoked"); void loadAdmin(root); }
          catch (e) { toast((e as Error).message, "bad"); }
        };
      });
      return;
    }

    if (adminTab === "roles") {
setHtml(area, `<table class="grid">
          <tr><th>Role</th>${adminScopes.map((s) => `<th>${esc(s)}</th>`).join("")}<th class="col-actions"></th></tr>
          ${adminRoles.map((r) => `
            <tr>
              <td class="tname">${esc(r.name)}${r.builtin ? ` <span class="badge">built-in</span>` : ""}</td>
              ${adminScopes.map((s) => `<td>${permMark(r.scopes?.[s])}</td>`).join("")}
              <td class="col-actions"><div class="row-actions">
                <button class="btn icon" data-editrole="${esc(r.name)}" title="Edit permissions">${ic.edit()}</button>
                ${r.builtin ? "" : `<button class="btn danger icon" data-delrole="${esc(r.name)}" title="Delete role">${ic.trash()}</button>`}
              </div></td>
            </tr>`).join("")}
        </table>
        <div class="actions"><button class="btn primary" id="add-role">${ic.plus(14)} New role</button></div>`);
      $("#add-role", area).onclick = () => openEditRole(null, root);
      area.querySelectorAll("[data-editrole]").forEach((b: Element) => {
        (b as HTMLElement).onclick = () => {
          const name = (b as HTMLElement).dataset.editrole!;
          const r = adminRoles.find((x) => x.name === name);
          if (r) openEditRole(r, root);
        };
      });
      area.querySelectorAll("[data-delrole]").forEach((b: Element) => {
        (b as HTMLElement).onclick = async () => {
          const name = (b as HTMLElement).dataset.delrole!;
          if (!await askConfirm("Delete role", `Delete the role "${name}"?`, "Delete", true)) return;
          try { await api.del(`/roles/${encodeURIComponent(name)}`); toast("Role deleted"); void loadAdmin(root); }
          catch (e) { toast((e as Error).message, "bad"); }
        };
      });
      return;
    }

    const users = await api.get<User[]>("/users");
setHtml(area, `<table class="grid">
        <tr><th>Username</th><th>Display Name</th><th>Role</th><th class="col-actions"></th></tr>
        ${users.map((u) => `
          <tr>
            <td>${esc(u.username)}${u.id === user?.id ? ` <span class="badge">you</span>` : ""}</td>
            <td>${esc(u.nickname)}</td>
            <td><span class="role-tag">${esc(u.role)}</span>${Object.keys(u.scopes ?? {}).length
              ? ` <span class="badge">overrides</span>` : ""}</td>
            <td class="col-actions"><div class="row-actions">
              <button class="btn icon" data-edituser="${u.id}" title="Edit role and permissions">${ic.edit()}</button>
              <button class="btn danger icon" data-deluser="${u.id}" title="Delete user">${ic.trash()}</button>
            </div></td>
          </tr>`).join("")}
      </table>
      <div class="actions"><button class="btn primary" id="add-user">${ic.plus(14)} Add user</button></div>`);
    $("#add-user", area).onclick = () => openAddUser(root);
    area.querySelectorAll("[data-edituser]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => {
        const id = (b as HTMLElement).dataset.edituser!;
        const u = users.find((x) => x.id === id);
        if (u) openEditUser(u, root);
      };
    });
    area.querySelectorAll("[data-deluser]").forEach((b: Element) => {
      (b as HTMLElement).onclick = async () => {
        const id = (b as HTMLElement).dataset.deluser!;
        const u = users.find((x) => x.id === id);
        if (!await askConfirm("Delete user", `Delete ${u?.username ?? "this user"}? Their API keys and sessions go too.`, "Delete", true)) return;
        try { await api.del(`/users/${id}`); toast("User removed"); void loadAdmin(root); }
        catch (e) { toast((e as Error).message, "bad"); }
      };
    });
  } catch (e) {
setHtml(area, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

/// Create or retune a role. `role` is null when adding a new one.
function openEditRole(role: RoleDef | null, root: Element) {
  const builtin = role?.builtin ?? false;
  const el = openModal(
    role ? `Role — ${esc(role.name)}` : "New role",
    `<label class="field"><span>Name</span>
       <input id="er-name" value="${esc(role?.name ?? "")}" placeholder="backup-operator" ${builtin ? "disabled" : ""}></label>
     <div class="section-title">Permissions</div>
     <div class="grid2 perm-grid">
       ${adminScopes.map((s) => `
         <label class="field"><span>${esc(s)}</span>
           ${permSelect("data-rolescope", s, role?.scopes?.[s] ?? "none")}</label>`).join("")}
     </div>`,
    `<button class="btn" id="er-cancel">Cancel</button>
     <button class="btn primary" id="er-save">Save</button>`,
  );
  $("#er-cancel", el).onclick = () => el.remove();
  $("#er-save", el).onclick = async () => {
    const scopes: Record<string, string> = {};
    el.querySelectorAll("[data-rolescope]").forEach((s: Element) => {
      scopes[(s as HTMLElement).dataset.rolescope!] = (s as HTMLSelectElement).value;
    });
    const name = $("#er-name", el).value.trim();
    if (!name) return toast("Role name is required", "bad");
    try {
      if (role) await api.put(`/roles/${encodeURIComponent(role.name)}`, { name, scopes });
      else await api.post("/roles", { name, scopes });
      toast(role ? "Role updated" : "Role created");
      el.remove();
      // Retuning a role can change your own effective permissions.
      user = await api.get<User>("/auth/me");
      if (!canAdmin()) {
        renderApp();
        return;
      }
      void loadAdmin(root);
    } catch (e) { toast((e as Error).message, "bad"); }
  };
}

/// Mint an API key for any user, with its own permission ceiling.
function openAddKey(users: User[], root: Element) {
  const el = openModal(
    "New API key",
    `<div class="grid2">
       <label class="field"><span>Name</span>
         <input id="nk-name" placeholder="leave blank for a random name">
         </label>
       <label class="field"><span>Owner</span><select id="nk-owner">
         ${users.map((u) => `<option value="${u.id}" ${u.id === user?.id ? "selected" : ""}>${esc(u.username)}</option>`).join("")}
       </select></label>
     </div>
     <div class="section-title">Permissions</div>
     <div class="grid2 perm-grid">
       ${adminScopes.map((s) => `
         <label class="field"><span>${esc(s)}</span>
           ${permSelect("data-keyscope", s, "", "inherit owner")}</label>`).join("")}
     </div>`,
    `<button class="btn" id="nk-cancel">Cancel</button>
     <button class="btn primary" id="nk-create">Create</button>`,
  );
  $("#nk-cancel", el).onclick = () => el.remove();
  $("#nk-create", el).onclick = async () => {
    const scopes: Record<string, string> = {};
    el.querySelectorAll("[data-keyscope]").forEach((s: Element) => {
      const v = (s as HTMLSelectElement).value;
      if (v) scopes[(s as HTMLElement).dataset.keyscope!] = v;
    });
    // Blank is allowed: the server generates a readable name.
    const name = $("#nk-name", el).value.trim();
    try {
      const k = await api.post<import("./api").ApiKey>("/keys", {
        name,
        user_id: ($("#nk-owner", el) as HTMLSelectElement).value,
        scopes,
      });
      el.remove();
      // Shown once and never again, so give it its own dismissable dialog
      // rather than a toast that disappears on a timer.
      showSecret(`API key — ${k.name}`, k.key ?? "");
      void loadAdmin(root);
    } catch (e) { toast((e as Error).message, "bad"); }
  };
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

function openEditUser(u: User, root: Element) {
  const el = openModal(
    `Edit — ${esc(u.username)}`,
    `<div class="grid2">
       <label class="field"><span>Display Name</span><input id="eu-nick" value="${esc(u.nickname)}"></label>
       <label class="field"><span>Role</span><select id="eu-role">
         ${adminRoles.map((r) =>
           `<option value="${esc(r.name)}" ${r.name === u.role ? "selected" : ""}>${esc(r.name)}</option>`).join("")}
       </select></label>
     </div>
     <label class="field"><span>Reset Password (blank = leave unchanged)</span>
       <input id="eu-pass" type="password" autocomplete="new-password" placeholder="min 8 characters"></label>
     <div class="section-title">Permission Overrides</div>
     <div class="grid2 perm-grid">
       ${adminScopes.map((s) => `
         <label class="field"><span>${esc(s)}</span>
           ${permSelect("data-scope", s, u.scopes?.[s] ?? "", "inherit from role")}</label>`).join("")}
     </div>`,
    `<button class="btn" id="eu-cancel">Cancel</button>
     <button class="btn primary" id="eu-save">Save</button>`,
  );
  $("#eu-cancel", el).onclick = () => el.remove();
  $("#eu-save", el).onclick = async () => {
    const scopes: Record<string, string> = {};
    el.querySelectorAll("[data-scope]").forEach((s: Element) => {
      const v = (s as HTMLSelectElement).value;
      if (v) scopes[(s as HTMLElement).dataset.scope!] = v;
    });
    const body: Record<string, unknown> = {
      nickname: $("#eu-nick", el).value,
      role: $("#eu-role", el).value,
      scopes,
    };
    const pass = $("#eu-pass", el).value;
    if (pass) body.password = pass;
    try {
      await api.put(`/users/${u.id}`, body);
      toast("User updated");
      el.remove();
      // Editing yourself can change your own role, so refresh the identity
      // the topbar and admin-button visibility are derived from.
      if (u.id === user?.id) {
        user = await api.get<User>("/auth/me");
        renderApp();
      } else {
        void loadAdmin(root);
      }
    } catch (e) { toast((e as Error).message, "bad"); }
  };
}

function openAddUser(root: Element) {
  const el = openModal(
    "Add user",
    `<div class="grid2">
      <label class="field"><span>Username</span><input id="nu-name" autocomplete="username"></label>
      <label class="field"><span>Password (min 8)</span><input id="nu-pass" type="password" autocomplete="new-password"></label>
      <label class="field"><span>Display Name</span><input id="nu-nick"></label>
      <label class="field"><span>Role</span><select id="nu-role">
        ${adminRoles.map((r) =>
          `<option value="${esc(r.name)}" ${r.name === "viewer" ? "selected" : ""}>${esc(r.name)}</option>`).join("")}
      </select></label>
    </div>`,
    `<button class="btn primary" id="nu-create">Create</button>`,
  );
  $("#nu-create", el).onclick = async () => {
    try {
      await api.post("/users", {
        username: $("#nu-name", el).value,
        password: $("#nu-pass", el).value,
        nickname: $("#nu-nick", el).value,
        role: $("#nu-role", el).value,
      });
      toast("User created");
      el.remove();
      void loadAdmin(root);
    } catch (e) { toast((e as Error).message, "bad"); }
  };
}

// ---------------------------------------------------------------- connection view

async function renderConn() {
  if (!selectedConn) return;
  const conn = connections.find((c) => c.id === selectedConn);
  if (!conn) return;
  const content = $("#content");
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
  content.querySelectorAll("[data-tab]").forEach((b: Element) => {
    (b as HTMLElement).onclick = () => {
      connTab = (b as HTMLElement).dataset.tab as typeof connTab;
      selectedVm = null;
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
    setHtml(body, `<div id="dash">loading…</div>`);
    $("#create-resource")?.addEventListener("click", () => openCreateResource());
    await loadDashboard();
  } else if (connTab === "vms") {
    setHtml(body, `<div id="vms-list" class="card-grid">loading…</div>`);
    $("#create-vm")?.addEventListener("click", () => openCreateVm());
    await loadVms();
  } else if (connTab === "networks") {
    setHtml(body, `<div id="nets-list" class="card-grid">loading…</div>`);
    $("#create-net")?.addEventListener("click", () => openCreateNetwork());
    await loadNetworks();
  } else if (connTab === "storage") {
    setHtml(body, `<div id="pools-list">loading…</div>`);
    renderedPools = []; // fresh container: force a full render on first load
    $("#create-pool")?.addEventListener("click", () => openCreatePool());
    await loadPools();
  } else {
    setHtml(body, `<div id="filters-list">loading…</div>`);
    await loadFilters();
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
    const { cpu, mem, cpuNow, memNow } = addHostSample(`host:${selectedConn}`, h);
    const memUsedKib = h.mem_total_kib - h.mem_available_kib;
    const activeNets = nets.filter((n) => n.active).length;
    const activePools = pools.filter((p) => p.active).length;

    setHtml(body, `
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
          ${sparkline(cpu)}
          <div class="chart-axis"><span>-1m</span><span>now</span></div>
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
          ${sparkline(mem, "mem")}
          <div class="chart-axis"><span>-1m</span><span>now</span></div>
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
          <tr><td>${esc(n.name)}</td><td>${esc(n.bridge)}</td>
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
        <div class="storage-card ${p.active ? "ok" : ""}">
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
  } catch (e) {
    setHtml(body, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
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
      </table>`);
    list.querySelectorAll("[data-fwedit]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => openFirewallEditor((b as HTMLElement).dataset.fwedit!);
    });
  } catch (e) {
    setHtml(list, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

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
  let rules: FirewallRule[] = [];
  let forwards: FirewallForward[] = [];
  let policy: FirewallPolicy = blankPolicy();

  const el = openModal(
    `Firewall — ${esc(netName)}`,
    `<div id="fw-body">loading…</div>`,
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
      <div class="actions"><button class="btn" id="fw-add-rule">${ic.plus(14)} Add rule</button></div>

      <div class="section-title fw-cat" style="margin-top:18px">NAT Port Forwards</div>
      <table class="grid">
        <tr><th>WAN</th><th>Proto</th><th>WAN Start</th><th>WAN End</th><th>Internal IP</th><th>Start</th><th>End</th><th></th></tr>
        ${forwards.map(fwFwdRow).join("") || `<tr><td colspan="8" class="empty">No port forwards.</td></tr>`}
      </table>
      <div class="actions"><button class="btn" id="fw-add-fwd">${ic.plus(14)} Add forward</button></div>`);

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
    const createCard = `
      <button class="storage-card create-card" id="vm-create-card">
        <span class="storage-card-icon">${ic.plus()}</span>
        <span class="storage-card-title">Create VM</span>
      </button>`;
    setHtml(list, vms.map((v) => `
      <div class="storage-card ${v.active ? "ok" : ""}" data-open data-vm-open="${esc(v.name)}">
        <div class="storage-card-head">
          <span class="storage-card-icon">${ic.vm()}</span>
          <span class="storage-card-title">${esc(v.name)}</span>
          <span class="badge ${v.active ? "on" : ""}">${v.active ? "Active" : "Inactive"}</span>
        </div>
        <div class="specs">
          <div class="spec"><b>vCPUs</b><span>${v.vcpus || "—"}</span></div>
          <div class="spec"><b>Memory</b><span>${v.memory_mb ? fmtBytes(v.memory_mb * 1024) : "—"}</span></div>
        </div>
        <div class="storage-card-actions net-actions">
          <button class="btn icon ${v.active ? "on" : ""}" data-vm-toggle="${esc(v.name)}" data-vm-action="${v.active ? "stop" : "start"}" title="${v.active ? "Shutdown" : "Start"}">${v.active ? ic.stop() : ic.start()}</button>
          <button class="btn icon" data-vm-console="${esc(v.name)}" title="Console">${ic.vm()}</button>
          <button class="btn icon" data-vm-edit="${esc(v.name)}" title="Edit hardware">${ic.edit()}</button>
          <button class="btn danger icon" data-vm-del="${esc(v.name)}" title="Delete">${ic.trash()}</button>
        </div>
      </div>`).join("") + createCard);
    const createBtn = $("#vm-create-card");
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
    const createCard = `
      <button class="storage-card net-card net-create-card" id="net-create-card">
        <span class="storage-card-icon">${ic.plus()}</span>
        <span class="storage-card-title">Create Network</span>
      </button>`;
    setHtml(list, nets.map((n) => `
      <div class="storage-card net-card ${n.active ? "ok" : ""}">
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
      </div>`).join("") + createCard);
    const createBtn = $("#net-create-card");
    if (createBtn) createBtn.onclick = () => openCreateNetwork();
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

function openCreateNetwork() {
  openNetworkForm(
    { name: "", forward: "nat", bridge: "", ip: "", dhcp_start: "", dhcp_end: "", dns: "" },
    "",
  );
}

/// One network form for create and edit. Fields appear and disappear with the
/// forward mode: bridge mode offers a bridge picker and no IP/DHCP/DNS (the
/// bridge owns addressing); NAT offers IP, DHCP and DNS forwarders; isolated
/// offers IP/DHCP but no bridge.
function openNetworkForm(initial: NetworkForm, uuid: string) {
  if (!selectedConn) return;
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
  api.get<string[]>(`/proxy/${selectedConn}/networks/bridges`)
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
    };
    if (!form.name) return toast("Name is required", "bad");
    if (form.forward === "bridge" && !form.bridge) {
      return toast("Select a bridge", "bad");
    }
    el.remove();
    const xml = networkXml(form, uuid);
    runTask(`${uuid ? "Update" : "Create"} network ${form.name}`, async () => {
      if (uuid) await api.putXml(`/proxy/${selectedConn}/networks/${encodeURIComponent(initial.name)}/xml`, xml);
      else await api.postXml(`/proxy/${selectedConn}/networks`, xml);
    });
    toast(`${uuid ? "Update" : "Creation"} started`);
    setTimeout(() => void renderConn(), 600);
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
    const shapeChanged = renderedPools.join("|") !== names.join("|");
    if (shapeChanged) {
      renderedPools = names;
      setHtml(list, pools.map((p) => {
        const used = p.allocation ?? 0;
        const cap = p.capacity ?? 0;
        const usedPct = cap > 0 ? Math.max(0, Math.min(100, (used / cap) * 100)) : 0;
        return `
      <div class="storage-card ${p.active ? "ok" : ""}" data-pool-card="${cssId(p.name)}">
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
      }).join("") || `<div class="empty">No pools.</div>`);
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

async function renderVm() {
  if (!selectedConn || !selectedVm) return;
  const content = $("#content");
  const vm = selectedVm;
setHtml(content, `
    <div class="dhead"><span class="dtitle">${esc(vm)}</span></div>
    <div class="tabbar">
      <button class="tab ${vmTab === "summary" ? "active" : ""}" data-vmtab="summary">${ic.datacenter(14)} Summary</button>
      <button class="tab ${vmTab === "hardware" ? "active" : ""}" data-vmtab="hardware">${ic.cpu(14)} Hardware</button>
      <button class="tab ${vmTab === "options" ? "active" : ""}" data-vmtab="options">${ic.options(14)} Options</button>
      <button class="tab ${vmTab === "firewall" ? "active" : ""}" data-vmtab="firewall">${ic.firewall(14)} Firewall</button>
      <button class="tab ${vmTab === "console" ? "active" : ""}" data-vmtab="console">${ic.vm(14)} Console</button>
      <button class="tab ${vmTab === "xml" ? "active" : ""}" data-vmtab="xml">${ic.xml(14)} XML</button>
      <div class="tabbar-actions row-actions">
        <button class="btn icon" id="vm-start" title="Start">${ic.start()}</button>
        <button class="btn icon" id="vm-shutdown" title="Shutdown">${ic.stop()}</button>
        <button class="btn icon" id="vm-reboot" title="Reboot">${ic.reboot()}</button>
        <button class="btn icon" id="vm-destroy" title="Force stop">${ic.forceStop()}</button>
        <button class="btn icon" id="vm-hwedit" title="Edit hardware">${ic.edit()}</button>
        <button class="btn danger icon" id="vm-delete" title="Delete">${ic.trash()}</button>
      </div>
    </div>
    <div id="vm-body"></div>`);
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
  $("#vm-start").onclick = () => act(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/start`, "POST").then(renderVm);
  $("#vm-shutdown").onclick = () => act(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/shutdown`, "POST").then(renderVm);
  $("#vm-destroy").onclick = () => act(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/destroy`, "POST").then(renderVm);
  $("#vm-reboot").onclick = () => act(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/reboot`, "POST").then(renderVm);
  $("#vm-hwedit").onclick = () => void openHardwareEditor({ vm });
  $("#vm-delete").onclick = () => deleteVm(vm);
  await renderVmBody(vm);
}

async function renderVmBody(vm: string) {
  if (!selectedConn) return;
  const body = $("#vm-body");
  if (vmTab === "console") {
    const zoomLabel = (z: number) => (z === 0 ? "Fit" : `${Math.round(z * 100)}%`);
    const vvUrl = `/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/console/vv`;
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
            <a class="btn primary" id="con-vv" href="${vvUrl}" download="${esc(vm)}.vv">${ic.download(14)} Open in virt-viewer</a>
            <p class="muted">Downloads a connection file. Add a VNC graphics device to use the built-in console.</p>
          </div>`);
        return;
      }

      const con = await attachConsole($("#console-stage"), kind, base, (msg) => {
        const m = $("#console-msg");
        if (m) {
          m.textContent = msg ?? "";
          m.classList.toggle("hidden", !msg);
        }
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

      setHint("Click the screen to grab input. Ctrl+wheel zooms.");

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

      setHtml(body, `
        <div class="actions">
          <button class="btn primary" id="opt-save">${ic.edit(14)} Save Options</button>
          <span class="hw-locked">Changes apply after the guest is restarted.</span>
        </div>
        <div class="grid2">
          <label class="field"><span>Name</span>
            <input id="opt-name" value="${esc(vm)}">
            </label>
          <label class="field"><span>Title</span>
            <input id="opt-title" value="${esc(title)}" placeholder="short label"></label>
        </div>
        <label class="field"><span>Description</span>
          <textarea id="opt-desc" rows="3" placeholder="free-form notes">${esc(description)}</textarea></label>
        <div class="grid2">
          <label class="field"><span>Boot Device</span>
            <select id="opt-boot">
              ${["hd", "cdrom", "network"].map((d) =>
                `<option value="${d}" ${d === bootDev ? "selected" : ""}>${d}</option>`).join("")}
            </select></label>
          <label class="field"><span>UUID</span>
            <input value="${esc(info.uuid ?? "—")}" disabled></label>
        </div>
        <div class="section-title" style="margin-top:14px">Lifecycle</div>
        <div class="specs">
          <div class="spec"><b>On Poweroff</b><span>${esc(onPoweroff)}</span></div>
          <div class="spec"><b>On Reboot</b><span>${esc(onReboot)}</span></div>
          <div class="spec"><b>On Crash</b><span>${esc(onCrash)}</span></div>
          <div class="spec"><b>Persistence</b><span>${esc(capLabel(autostartHint))}</span></div>
          <div class="spec"><b>Machine</b><span>${esc(machine)}</span></div>
          <div class="spec"><b>Architecture</b><span>${esc(arch)}</span></div>
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
        runTask(`Update options for ${target}`, async () => {
          await api.putXml(`/proxy/${selectedConn}/domains/${encodeURIComponent(target)}/xml`, next);
        });
        toast("Options update started");
      });
    } catch (e) {
      setHtml(body, `<div class="empty">${esc((e as Error).message)}</div>`);
    }
    return;
  }

  if (vmTab === "firewall") {
    // Inline rules editor, scoped to this guest by its interface MACs.
    const rulesBody = body;
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
        </div>`);
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
    try {
      const [{ xml }, info] = await Promise.all([
        api.get<{ xml: string }>(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/xml`),
        api.get<any>(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}`),
      ]);
      const devices = devicesFromXml(xml);
      const cpus = xml.match(/<vcpu[^>]*>(\d+)<\/vcpu>/)?.[1] ?? "?";
      const memMb = Math.round(+(xml.match(/<memory[^>]*>(\d+)<\/memory>/)?.[1] ?? 0) / 1024);
      const fw = /pflash|OVMF/i.test(xml) ? "UEFI (OVMF)" : "BIOS (SeaBIOS)";
      const gfx = /<graphics[^>]*type=['"]spice/.test(xml) ? "spice" : "vnc";
      const running = info.active === true;

      const fixed = [
        { ico: ic.cpu(), label: "Processors", val: `${cpus} vCPU` },
        { ico: ic.memory(), label: "Memory", val: `${memMb} MB` },
        { ico: ic.cpu(), label: "BIOS", val: fw },
        { ico: ic.vm(), label: "Display", val: gfx },
      ];
      const rows = fixed.map((r) => `
        <div class="hwrow">
          <span class="hwrow-ic hw-ic">${r.ico}</span>
          <span class="hwrow-label">${esc(r.label)}</span>
          <span class="hwctl hwctl-static">${esc(r.val)}</span>
          <span></span>
        </div>`).join("");
      const devRows = devices.map((d) => {
        const t = hwType(d.kind);
        return `
        <div class="hwrow">
          <span class="hwrow-ic hw-ic">${ic[t?.icon ?? "cpu"]()}</span>
          <span class="hwrow-label">${esc(t?.label ?? d.kind)}</span>
          <span class="hwctl hwctl-static">${esc(deviceSummary(d))}</span>
          <span></span>
        </div>`;
      }).join("");

      setHtml(body, `
        <div class="actions hw-actions">
          <button class="btn primary" id="hw-edit">${ic.edit(14)} Edit hardware</button>
          ${running ? `<span class="hw-locked">Changes apply after the guest is restarted.</span>` : ""}
        </div>
        <div class="hwtable">${rows}${devRows}</div>
        `);
      $("#hw-edit")?.addEventListener("click", () => void openHardwareEditor({ vm }));
    } catch (e) {
      setHtml(body, `<div class="empty">${esc((e as Error).message)}</div>`);
    }
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
  // summary: live CPU/memory metrics with sparklines, then the facts grid.
  try {
    const [d, g, s] = await Promise.all([
      api.get<any>(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}`),
      api.get<{ graphics: { kind: string; host: string; port: number } | null }>(
        `/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/graphics`),
      api.get<DomainStats>(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/stats`),
    ]);

    const { cpu, mem, cpuNow, memNow } = addSample(s);
    const running = s.state === "running";
    const usedKib = s.usable_kib != null
      ? Math.max(0, s.memory_kib - s.usable_kib)
      : (s.rss_kib ?? s.memory_kib);

    setHtml(body, `
    <div class="sumgrid vm-sumgrid">
      <div class="metric ${running ? "ok" : ""}">
        <div class="metric-head">
          <span class="metric-ic">${ic.cpu()}</span>
          <b>CPU</b>
          <span class="metric-value">${pct(cpuNow)}</span>
        </div>
        <div class="meter"><div class="meter-fill ${cpuNow > 85 ? "hot" : ""}" style="width:${cpuNow.toFixed(1)}%"></div></div>
        <div class="chart">
          ${sparkline(cpu)}
          <div class="chart-axis"><span>-1m</span><span>now</span></div>
        </div>
        <div class="metric-foot"><span>${s.vcpus} vCPU</span><span>host ${s.host.cpus} CPU</span></div>
      </div>

      <div class="metric ${running ? "ok" : ""}">
        <div class="metric-head">
          <span class="metric-ic">${ic.memory()}</span>
          <b>Memory</b>
          <span class="metric-value">${pct(memNow)}</span>
        </div>
        <div class="meter"><div class="meter-fill ${memNow > 85 ? "hot" : ""}" style="width:${memNow.toFixed(1)}%"></div></div>
        <div class="chart">
          ${sparkline(mem, "mem")}
          <div class="chart-axis"><span>-1m</span><span>now</span></div>
        </div>
        <div class="metric-foot">
          <span>${fmtBytes(usedKib * 1024)} used</span>
          <span>${fmtBytes(s.max_memory_kib * 1024)} max</span>
        </div>
      </div>
    </div>

    <div class="specs">
      <div class="spec"><b>State</b><span>${esc(capLabel(d.state))}</span></div>
      <div class="spec"><b>ID</b><span>${d.id}</span></div>
      <div class="spec"><b>UUID</b><span>${esc(d.uuid ?? "—")}</span></div>
      <div class="spec"><b>Persistent</b><span>${d.persistent}</span></div>
      <div class="spec"><b>Graphics</b><span>${esc(g.graphics?.kind ?? "none")}${g.graphics ? ` ${esc(g.graphics.host)}:${g.graphics.port}` : ""}</span></div>
    </div>`);
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

async function deleteVm(name: string) {
  if (!selectedConn) return;
  if (!(await askConfirm("Delete VM", `Delete VM "${name}"?`, "Delete", true))) return;
  const removeStorage = await askConfirm(
    "Remove storage",
    `Also delete the disks belonging to "${name}"? This cannot be undone.`,
    "Delete disks",
    true,
  );
  try {
    await api.del(`/proxy/${selectedConn}/domains/${encodeURIComponent(name)}?remove_storage=${removeStorage}`);
    toast("Deleted");
    selectedVm = null;
    renderConn();
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
    renderDc();
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
  renderDc();
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
) {
  for (let i = 0; i < devices.length; i++) {
    const d = devices[i];
    if (d.kind !== "disk" || (d.fields.mode ?? "existing") !== "new") continue;
    const pool = (d.fields.pool ?? "").trim();
    const sizeGb = Math.max(1, +(d.fields.size_gb ?? "") || 1);
    const format = (d.fields.format ?? "qcow2") === "raw" ? "raw" : "qcow2";
    const name = `${vmName}-disk${i + 1}-${sizeGb}gb.${format}`;
    set(`creating ${sizeGb} GB volume ${name}`);
    await api.post(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes`, {
      xml: volumeXml({ name, capacity_gb: sizeGb, format }),
    });
    d.fields.volume = name;
    delete d.fields.mode;
  }
}

/// Proxmox-style hardware editor: device list on the LEFT, the selected
/// device's form on the right. Used for both "Create VM" and editing an
/// existing guest's hardware, so the two can never drift apart.
async function openHardwareEditor(opts: { vm?: string } = {}) {
  if (!selectedConn) return;
  const conn = selectedConn;
  const editing = !!opts.vm;

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
  const newDisk = { pool: "", size_gb: 10, format: "qcow2" as "raw" | "qcow2" };

  const el = openModal(
    editing ? `Hardware — ${esc(opts.vm ?? "")}` : "Create VM",
    "",
    `<button class="btn" id="hw-cancel">Cancel</button>
     <button class="btn primary" id="hw-save">${editing ? "Save" : "Create"}</button>`,
  );
  el.querySelector(".modal")?.classList.add("hw-modal");

  const { pools, nets } = await loadFormData(conn);
  // Volumes per pool, fetched lazily and cached for the device forms.
  const volCache: Record<string, string[]> = {};
  const volumes = async (pool: string): Promise<string[]> => {
    if (!pool) return [];
    if (!volCache[pool]) {
      try {
        const v = await api.get<VolumeSummary[]>(
          `/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes`);
        volCache[pool] = v.map((x) => x.name);
      } catch {
        volCache[pool] = [];
      }
    }
    return volCache[pool];
  };
  newDisk.pool = pools[0]?.name ?? "";

  // New VMs start with an empty CD/DVD drive, so attaching an ISO (or leaving
  // the drive empty) is one click away instead of buried in Add Hardware.
  if (!editing) {
    devices = [{ kind: "cdrom", fields: { pool: newDisk.pool, volume: "" } }];
  }

  let page = "general";

  const opt = (list: string[], sel: string) =>
    list.map((v) => `<option value="${esc(v)}" ${v === sel ? "selected" : ""}>${esc(v)}</option>`).join("");

  /** Sidebar rows mirror Proxmox: label plus the current value. */
  function sidebar(): string {
    const fixed = [
      { key: "general", icon: ic.datacenter(), label: "General", sum: general.name || "unnamed" },
      { key: "processors", icon: ic.cpu(), label: "Processors", sum: `${general.cpus} vCPU` },
      { key: "memory", icon: ic.memory(), label: "Memory", sum: `${general.memory_mb} MB` },
      ...(editing ? [] : [{ key: "disk", icon: ic.disk(), label: "Hard Disk", sum: `${newDisk.size_gb} GB ${newDisk.format}` }]),
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
      const fields = await Promise.all(t.fields.map(async (f) => {
        const val = d.fields[f.key] ?? f.value ?? "";
        if (f.type === "select") {
          let list: string[] = (f.options ?? []).map((o) => o.value);
          if (f.key === "pool") list = pools.map((p) => p.name);
          if (f.key === "network") list = nets.map((n) => n.name);
          if (f.key === "volume") {
            list = await volumes(d.fields.pool ?? pools[0]?.name ?? "");
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
      case "disk":
        return `<div class="hw-page active">
          <h3 class="hw-title">Hard Disk</h3>
          <label class="field"><span>Storage Pool</span>
            <select id="g-pool">${opt(pools.map((p) => p.name), newDisk.pool)}</select></label>
          <label class="field"><span>Disk Size (GB)</span>
            <input id="g-size" type="number" min="1" value="${newDisk.size_gb}"></label>
          <label class="field"><span>Format</span>
            <select id="g-format">${opt(["qcow2", "raw"], newDisk.format)}</select></label>
        </div>`;
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
    setHtml(el.querySelector(".modal-body")!, `<div class="hw-body">
      <nav class="hw-side" id="hw-side">${sidebar()}</nav>
      <div class="hw-pane" id="hw-pane">${await pageHtml()}</div>
    </div>`);
    // The body is a grid host for the editor, so drop the default padding.
    (el.querySelector(".modal-body") as HTMLElement).style.padding = "0";
    (el.querySelector(".modal-body") as HTMLElement).style.overflow = "hidden";

    el.querySelectorAll("[data-hw]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => {
        page = (b as HTMLElement).dataset.hw!;
        void render();
      };
    });
    $("#hw-add", el).onclick = () => openAddHardware();

    // General/spec fields write straight back into the model.
    const bind = (sel: string, fn: (v: string) => void) => {
      const n = $(sel, el);
      if (!n) return;
      n.onchange = () => { fn(n.value); void render(); };
    };
    bind("#g-name", (v) => { general.name = v; });
    bind("#g-cpus", (v) => { general.cpus = Math.max(1, +v || 1); });
    bind("#g-mem", (v) => { general.memory_mb = Math.max(128, +v || 128); });
    bind("#g-gfx", (v) => { general.graphics = v as "vnc" | "spice"; });
    bind("#g-fw", (v) => { general.firmware = v as "uefi" | "bios"; });
    bind("#g-pool", (v) => { newDisk.pool = v; });
    bind("#g-size", (v) => { newDisk.size_gb = Math.max(1, +v || 1); });
    bind("#g-format", (v) => { newDisk.format = v as "raw" | "qcow2"; });

    // Device field edits, plus removal.
    if (page.startsWith("dev-")) {
      const i = +page.slice(4);
      el.querySelectorAll("[data-field]").forEach((n: Element) => {
        (n as HTMLElement).onchange = () => {
          const key = (n as HTMLElement).dataset.field!;
          devices[i].fields[key] = (n as HTMLInputElement).value;
          // Changing the pool invalidates the volume list.
          void render();
        };
      });
      const rm = $("#hw-remove", el);
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
          if (f.key === "pool") list = pools.map((p) => p.name);
          if (f.key === "network") list = nets.map((n) => n.name);
          if (f.key === "volume") {
            list = await volumes(draft.pool ?? pools[0]?.name ?? "");
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
          ${HW_CATALOG.map((t2) => `
            <button class="add-hw-type ${t2.id === kind ? "active" : ""}" data-at="${t2.id}">
              <b>${esc(t2.label)}</b><small>${esc(t2.blurb)}</small>
            </button>`).join("")}
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

  $("#hw-cancel", el).onclick = () => el.remove();
  $("#hw-save", el).onclick = async () => {
    for (const d of devices) {
      const bad = validateDevice(d);
      if (bad) return toast(bad, "bad");
    }

    if (editing && opts.vm) {
      const oldName = opts.vm;
      const renamed = general.name.trim() && general.name.trim() !== oldName;
      el.remove();
      runTask(`Update ${general.name.trim() || oldName}`, async (set) => {
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
          .replace(/<memballoon\b[^>]*\/>\s*/g, "")
          .replace(/<channel\b[\s\S]*?org\.qemu\.guest_agent[\s\S]*?<\/channel>\s*/g, "")
          .replace(/<tpm\b[\s\S]*?<\/tpm>\s*/g, "")
          .replace(/<watchdog\b[^>]*\/?>\s*/g, "")
          .replace(/<vcpu([^>]*)>\d+<\/vcpu>/, `<vcpu$1>${general.cpus}</vcpu>`)
          .replace(/<memory([^>]*)>\d+<\/memory>/, `<memory$1>${general.memory_mb * 1024}</memory>`)
          .replace(/<currentMemory([^>]*)>\d+<\/currentMemory>/, `<currentMemory$1>${general.memory_mb * 1024}</currentMemory>`)
          .replace(/<name>[^<]*<\/name>/, `<name>${esc(target)}</name>`);
        set("defining domain");
        await api.putXml(`/proxy/${conn}/domains/${encodeURIComponent(target)}/xml`,
          withDevices(stripped, devices));
        await refreshTreeVms();
      });
      toast("Hardware update started");
      setTimeout(() => void renderVm(), 600);
      return;
    }

    if (!general.name.trim()) return toast("Name is required", "bad");
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
    el.remove();
    runTask(`Create VM ${form.name}`, async (set) => {
      set(`creating ${newDisk.size_gb} GB volume`);
      await api.post(`/proxy/${conn}/pools/${encodeURIComponent(newDisk.pool)}/volumes`, {
        xml: volumeXml({ name: `${form.name}.qcow2`, capacity_gb: newDisk.size_gb, format: newDisk.format }),
      });
      // Create any additional "new volume" disks before defining the domain.
      await createPendingVolumes(conn, form.name, devices, set);
      set("defining domain");
      await api.postXml(`/proxy/${conn}/domains?start=true`, withDevices(domainXml(form), devices));
    });
    toast("VM creation started");
    renderConn();
  };

  void render();
}

/// Back-compat entry point used by the VMs tab button.
function openCreateVm() {
  if (selectedConn) {
    void openHardwareEditor();
    return;
  }
  // No daemon selected (e.g. the Datacenter Guests tab): pick one first.
  void pickDaemon().then((id) => {
    if (!id) return;
    selectedConn = id;
    renderTree();
    void openHardwareEditor();
  });
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

/// Dashboard "Create Resource": pick VM / Network / Storage Pool, then hand
/// off to the existing create flows.
function openCreateResource() {
  if (!selectedConn) return;
  const el = openModal(
    "Create Resource",
    `<div class="create-resource-grid">
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
    </div>`,
    `<button class="btn" id="cr-cancel">Cancel</button>`,
  );
  $("#cr-cancel", el).onclick = () => el.remove();
  el.querySelectorAll("[data-create]").forEach((b: Element) => {
    (b as HTMLElement).onclick = () => {
      const kind = (b as HTMLElement).dataset.create!;
      el.remove();
      if (kind === "vm") openCreateVm();
      else if (kind === "network") openCreateNetwork();
      else openCreatePool();
    };
  });
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
    runTask(`Create pool ${form.name}`, async () => {
      await api.postXml(`/proxy/${selectedConn}/pools`, poolXml(form));
    });
    toast("Pool creation started");
    renderConn();
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
    runTask(`Create volume ${name}`, async () => {
      await api.post(`/proxy/${selectedConn}/pools/${encodeURIComponent(pool)}/volumes`, {
        xml: volumeXml({ name, capacity_gb: size, format }),
      });
    });
    toast("Volume creation started");
  };
}

function openUpload(pool: string) {
  if (!selectedConn) return;
  const el = openModal(
    `Upload ISO to ${pool}`,
    `<label class="field"><span>Volume Name</span><input id="up-name" placeholder="arch.iso"></label>
     <label class="field"><span>File</span><input id="up-file" type="file"></label>
     <p class="hint" id="up-progress"></p>`,
    `<button class="btn primary" id="up-go">Upload</button>`,
  );
  $("#up-go", el).onclick = async () => {
    const file = ($("#up-file", el) as HTMLInputElement).files?.[0];
    const name = $("#up-name", el).value;
    if (!file || !name) return toast("Name and file required", "bad");
    el.remove();
    runTask(`Upload ${name}`, async (set) => {
      set("uploading…");
      const r = await fetch(`/proxy/${selectedConn}/pools/${encodeURIComponent(pool)}/volumes/${encodeURIComponent(name)}/upload?name=${encodeURIComponent(name)}`, {
        method: "POST",
        credentials: "same-origin",
        headers: { "Content-Type": "application/octet-stream" },
        body: file,
      });
      if (!r.ok) {
        const j = await r.json().catch(() => ({}));
        throw new Error((j as any).error ?? r.statusText);
      }
    });
    toast("Upload started");
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
         <span id="pf-who-user">@${esc(user.username)} · ${esc(user.role)}</span>
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
     <div class="actions"><button class="btn" id="pf-newkey">${ic.key()} New API key</button></div>`,
    // Save sits next to Cancel and only appears once something differs, so the
    // footer never offers a no-op. Cancel/Close is the same button relabelled:
    // with pending edits, closing discards them.
    `<button class="btn" id="pf-close">Close</button>
     <button class="btn primary hidden" id="pf-save">Save Changes</button>`,
  );
  $("#pf-close", el).onclick = () => el.remove();
  $("#pf-avatar-btn", el).onclick = () => void changeAvatar(el);

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
function refreshProfileCard(el: Element) {
  if (!user) return;
  const name = $("#pf-who-name", el);
  if (name) name.textContent = user.nickname;
  const who = $("#pf-who-user", el);
  if (who) who.textContent = `@${user.username} · ${user.role}`;
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

function renderTasks() {
  const tasks = allTasks();
  const box = $("#tasks-body");
  const dock = $("#tasks");
  if (!box || !dock) return;
  dock.classList.toggle("hidden", tasks.length === 0);
setHtml(box, tasks.map((t) => `
    <div class="task-row">
      <span class="task-dot ${t.status}"></span>
      <span><b>${esc(t.label)}</b><br><span class="task-msg">${esc(t.message)}</span></span>
      <button class="btn icon" data-clear="${t.id}">${ic.close()}</button>
    </div>`).join(""));
  box.querySelectorAll("[data-clear]").forEach((b: Element) => {
    (b as HTMLElement).onclick = () => clearTask(+(b as HTMLElement).dataset.clear!);
  });
}

// ---------------------------------------------------------------- polling

function startPolling() {
  if (pollTimer) window.clearInterval(pollTimer);
  pollTimer = window.setInterval(() => {
    if (!user) return;
    // Keep the sidebar's guest states live regardless of the open pane.
    void refreshTreeVms();
    if (!selectedConn && !selectedVm && dcTab === "dashboard") {
      loadDcDashboard();
    } else if (selectedConn && !selectedVm) {
      if (connTab === "dashboard") loadDashboard();
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
