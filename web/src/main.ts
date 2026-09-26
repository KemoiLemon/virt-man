import { api } from "./api";
import type { Connection, DomainSummary, NetworkSummary, PoolSummary, User, VolumeSummary } from "./api";
import { attachConsole, isTauri, tauriWindow } from "./console";
import DOMPurify from "dompurify";
import { ic } from "./icons";
import { HW_CATALOG, deviceSummary, deviceXml, devicesFromXml, hwType, validateDevice, withDevices } from "./hardware";
import type { HwDevice } from "./hardware";
import { enhanceSelects } from "./listbox";
import { addSample, fmtBytes, pct, sparkline } from "./stats";
import type { DomainStats } from "./stats";
import { allTasks, clearTask, onTasks, runTask } from "./task";
import { domainXml, esc, networkXml, poolXml, volumeXml } from "./xml";
import type { DomainForm, NetworkForm, PoolForm } from "./xml";

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
let connTab: "vms" | "networks" | "storage" = "vms";
let selectedVm: string | null = null;
let vmTab: "summary" | "hardware" | "console" | "xml" = "summary";
let dcTab: "servers" | "users" = "servers";
let pollTimer: number | null = null;
/// VMs per connection, so the sidebar tree can nest guests under their server
/// without every render refetching.
const treeVms: Record<string, DomainSummary[]> = {};

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
        <p class="hint">First run — create the admin account.</p>
        <label class="field"><span>Username</span><input id="su" autocomplete="username"></label>
        <label class="field"><span>Password (min 8 chars)</span><input id="sp" type="password" autocomplete="new-password"></label>
        <label class="field"><span>Nickname</span><input id="sn"></label>
      </div>
      <div class="modal-foot"><button class="btn primary" id="setup-btn">Create admin</button></div>
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
      <div class="modal-foot"><button class="btn primary" id="login-btn">Sign in</button></div>
    </div>`);
  $("#login-btn").onclick = async () => {
    try {
      user = await api.post<User>("/auth/login", { username: $("#lu").value, password: $("#lp").value });
      renderApp();
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };
  $("#lp").onkeydown = (e: KeyboardEvent) => { if (e.key === "Enter") $("#login-btn").click(); };
}

async function logout() {
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
      <span class="host">${isTauri() ? "desktop" : "web"}${authDisabled ? " · local" : ` · ${esc(user.nickname)}`}</span>
      <button class="btn icon" id="theme-btn" title="Toggle theme">${ic.theme()}</button>
      <button class="btn" id="webui-btn">${ic.webui()} Connect to WebUI</button>
      ${authDisabled ? "" : `<button class="btn" id="logout-btn">Sign out</button>`}
      ${isTauri() ? `<span class="window-controls">
        <button class="win-btn" id="win-min" title="Minimise">${ic.min()}</button>
        <button class="win-btn" id="win-max" title="Maximise">${ic.max()}</button>
        <button class="win-btn close" id="win-close" title="Close">${ic.close()}</button>
      </span>` : ""}
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
  $("#webui-btn").onclick = connectToWebui;
  $("#add-conn-row").onclick = () => openConnectionDialog();
  $("#tasks-close").onclick = () => $("#tasks").classList.add("hidden");

  onTasks(renderTasks);
  renderTree();
  renderProfileBtn();
  wireWindowChrome();
  if (!selectedConn) renderDc();
  // Populate the auto-expanded tree immediately rather than waiting for the
  // first poll tick or a server click.
  void refreshTreeVms();
  startPolling();
}

function wireWindowChrome() {
  if (!isTauri()) return;
  const w = tauriWindow();
  $("#win-min")?.addEventListener("click", () => { void w?.minimize(); });
  $("#win-max")?.addEventListener("click", async () => {
    if (!w) return;
    if (await w.isMaximized()) await w.unmaximize();
    else await w.maximize();
  });
  $("#win-close")?.addEventListener("click", () => { void w?.close(); });

  // Dragging: `data-tauri-drag-region` only fires for the exact element under
  // the cursor, so flex gaps and nested spans in the topbar are dead zones.
  // Start the drag explicitly for any press that is not on a control.
  const bar = document.querySelector(".topbar");
  if (!bar) return;
  const interactive = "button, input, select, textarea, a";
  bar.addEventListener("mousedown", (evt: Event) => {
    const e = evt as MouseEvent;
    if (e.button !== 0) return;
    if ((e.target as Element).closest?.(interactive)) return;
    e.preventDefault();
    void w?.startDragging();
  });
  // Double-clicking the titlebar toggles maximise, as users expect.
  bar.addEventListener("dblclick", (evt: Event) => {
    const e = evt as MouseEvent;
    if ((e.target as Element).closest?.(interactive)) return;
    void (async () => {
      if (!w) return;
      if (await w.isMaximized()) await w.unmaximize();
      else await w.maximize();
    })();
  });
}

/// Navigate the app window to a remote web UI. Standalone desktop builds use
/// this instead of local connections; the remote web UI is the same software.
async function connectToWebui() {
  const url = localStorage.getItem("virt-man-webui") ?? "https://localhost:8006";
  const next = await askText("Connect to WebUI", "Web UI URL", url, "https://host:8006");
  if (!next) return;
  let target: URL;
  try {
    target = new URL(next);
  } catch {
    toast("Invalid URL", "bad");
    return;
  }
  if (target.protocol !== "https:" && target.protocol !== "http:") {
    toast("Only http(s) URLs are allowed", "bad");
    return;
  }
  localStorage.setItem("virt-man-webui", target.toString());
  // pi-lens-ignore: no-open-redirect-js
  location.href = target.toString();
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
      const guestRows =
        vms.length
          ? vms
              .map(
                (v) => `
      <button class="tree-row tree-vm ${selectedVm === v.name ? "sel" : ""}" data-vm="${esc(v.name)}">
        <span class="tree-ic ${v.active ? "on" : ""}">${ic.vm()}</span>
        <span class="tree-label">${esc(v.name)}</span>
        <span class="tree-state ${v.active ? "on" : ""}">${esc(v.state)}</span>
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
    selectedConn = null;
    selectedVm = null;
    dcTab = "servers";
    renderTree();
    renderDc();
  };
  tree.querySelectorAll("[data-conn]").forEach((row: Element) => {
    (row as HTMLElement).onclick = () => {
      selectedConn = (row as HTMLElement).dataset.conn!;
      selectedVm = null;
      connTab = "vms";
      renderTree();
      renderConn();
      void refreshTreeVms();
    };
  });
  tree.querySelectorAll("[data-vm]").forEach((row: Element) => {
    (row as HTMLElement).onclick = () => {
      selectedVm = (row as HTMLElement).dataset.vm!;
      vmTab = "summary";
      renderTree();
      renderVm();
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
  if (changed) renderTree();
}

function renderProfileBtn() {
  const el = $("#profile-btn");
  if (!el || !user) return;
  const avatar = user.avatar
    ? `<img src="/avatars/${encodeURIComponent(user.avatar)}" alt="">`
    : (user.nickname[0] ?? "u").toUpperCase();
  el.onclick = openProfile;
  setHtml(el, `
    <div class="avatar">${avatar}</div>
    <div class="who"><b>${esc(user.nickname)}</b><span>${user.role}</span></div>`);
}

// ---------------------------------------------------------------- datacenter

function renderDc() {
  const content = $("#content");
  // Real tabbar; the Users tab is hidden when the local app disables auth.
  // Actions live on the tab bar itself, right-aligned after the tabs.
  const tabs = `<div class="tabbar">
      <button class="tab ${dcTab === "servers" ? "active" : ""}" data-dctab="servers">${ic.server(14)} Servers</button>
      ${authDisabled ? "" : `<button class="tab ${dcTab === "users" ? "active" : ""}" data-dctab="users">${ic.users(14)} Users &amp; API keys</button>`}
      ${dcTab === "servers"
        ? `<div class="tabbar-actions"><button class="btn primary" id="dc-add">${ic.plus(14)} Add Server</button></div>`
        : ""}
    </div>`;
  // Aggregate cards across every known server, from the tree's guest cache.
  const allVms = Object.values(treeVms).flat();
  const runningVms = allVms.filter((v) => v.active).length;
  const cards = dcTab === "servers"
    ? `<div class="cards">
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
      </div>`
    : "";
  const body = dcTab === "servers"
    ? `${cards}<div class="section-title">Servers</div>
       <table class="grid">
         <tr><th>Name</th><th>URL</th><th>Cert</th><th class="col-actions"></th></tr>
         ${connections.map((c) => `
           <tr>
             <td>${esc(c.name)}</td>
             <td>${esc(c.url)}</td>
             <td>${c.fingerprint ? c.fingerprint.slice(0, 17) + "…" : "—"}</td>
             <td class="col-actions">
               <div class="row-actions">
                 <button class="btn icon" data-edit="${c.id}" title="Edit server">${ic.edit()}</button>
                 <button class="btn danger icon" data-del="${c.id}" title="Remove server">${ic.trash()}</button>
               </div>
             </td>
           </tr>`).join("") || `<tr><td colspan="4" class="empty">No servers yet — add one.</td></tr>`}
       </table>`
    : `<div id="users-area">loading…</div>`;
setHtml(content, `<div class="dhead"><span class="dtitle">Datacenter</span></div>${tabs}${body}`);

  $("#dc-add")?.addEventListener("click", () => openConnectionDialog());
  content.querySelectorAll("[data-dctab]").forEach((b: Element) => {
    (b as HTMLElement).onclick = () => {
      dcTab = (b as HTMLElement).dataset.dctab as typeof dcTab;
      renderDc();
    };
  });
  content.querySelectorAll("[data-edit]").forEach((b: Element) => {
    (b as HTMLElement).onclick = () => openConnectionDialog((b as HTMLElement).dataset.edit!);
  });
  content.querySelectorAll("[data-del]").forEach((b: Element) => {
    (b as HTMLElement).onclick = () => deleteConnection((b as HTMLElement).dataset.del!);
  });
  if (dcTab === "users") loadUsers();
}

// ---------------------------------------------------------------- users

async function loadUsers() {
  const area = $("#users-area");
  if (!area) return;
  try {
    const [users, keys] = await Promise.all([
      api.get<User[]>("/users"),
      api.get<import("./api").ApiKey[]>("/keys"),
    ]);
setHtml(area, `<div class="section-title">Users</div>
      <table class="grid">
        <tr><th>Username</th><th>Nickname</th><th>Role</th><th></th></tr>
        ${users.map((u) => `
          <tr><td>${esc(u.username)}</td><td>${esc(u.nickname)}</td><td>${esc(u.role)}</td>
          <td><button class="btn danger icon" data-deluser="${u.id}">${ic.trash()}</button></td></tr>`).join("")}
      </table>
      <div class="actions"><button class="btn primary" id="add-user">Add user</button></div>
      <div class="section-title">API keys</div>
      <table class="grid">
        <tr><th>Name</th><th>Prefix</th><th></th></tr>
        ${keys.map((k) => `
          <tr><td>${esc(k.name)}</td><td>${esc(k.prefix)}…</td>
          <td><button class="btn danger icon" data-delkey="${k.id}">${ic.trash()}</button></td></tr>`).join("") || `<tr><td colspan="3" class="empty">No keys.</td></tr>`}
      </table>`);
    $("#add-user", area).onclick = () => openAddUser();
    area.querySelectorAll("[data-deluser]").forEach((b: Element) => {
      (b as HTMLElement).onclick = async () => {
        const id = (b as HTMLElement).dataset.deluser!;
        try { await api.del(`/users/${id}`); toast("User removed"); loadUsers(); }
        catch (e) { toast((e as Error).message, "bad"); }
      };
    });
    area.querySelectorAll("[data-delkey]").forEach((b: Element) => {
      (b as HTMLElement).onclick = async () => {
        const id = (b as HTMLElement).dataset.delkey!;
        try { await api.del(`/keys/${id}`); toast("Key revoked"); loadUsers(); }
        catch (e) { toast((e as Error).message, "bad"); }
      };
    });
  } catch (e) {
setHtml(area, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

function openAddUser() {
  const el = openModal(
    "Add user",
    `<div class="grid2">
      <label class="field"><span>Username</span><input id="nu-name"></label>
      <label class="field"><span>Password (min 8)</span><input id="nu-pass" type="password"></label>
      <label class="field"><span>Nickname</span><input id="nu-nick"></label>
      <label class="field"><span>Role</span><select id="nu-role">
        <option value="viewer">viewer</option><option value="operator">operator</option><option value="admin">admin</option>
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
      loadUsers();
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
      <button class="tab ${connTab === "vms" ? "active" : ""}" data-tab="vms">${ic.vms(14)} VMs</button>
      <button class="tab ${connTab === "networks" ? "active" : ""}" data-tab="networks">${ic.network(14)} Networks</button>
      <button class="tab ${connTab === "storage" ? "active" : ""}" data-tab="storage">${ic.storage(14)} Storage</button>
      <div class="tabbar-actions">
        ${connTab === "vms" ? `<button class="btn primary" id="create-vm">${ic.plus(14)} Create VM</button>` : ""}
        ${connTab === "networks" ? `<button class="btn primary" id="create-net">${ic.plus(14)} Create network</button>` : ""}
        ${connTab === "storage" ? `<button class="btn primary" id="create-pool">${ic.plus(14)} Create pool</button>` : ""}
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
  if (connTab === "vms") {
    setHtml(body, `<div id="vms-list">loading…</div>`);
    $("#create-vm")?.addEventListener("click", () => openCreateVm());
    await loadVms();
  } else if (connTab === "networks") {
    setHtml(body, `<div id="nets-list">loading…</div>`);
    $("#create-net")?.addEventListener("click", () => openCreateNetwork());
    await loadNetworks();
  } else {
    setHtml(body, `<div id="pools-list">loading…</div>`);
    $("#create-pool")?.addEventListener("click", () => openCreatePool());
    await loadPools();
  }
}

async function loadVms() {
  if (!selectedConn) return;
  try {
    const vms = await api.get<DomainSummary[]>(`/proxy/${selectedConn}/domains`);
    const list = $("#vms-list");
    if (!list) return;
setHtml(list, `<table class="grid">
      <tr><th>Name</th><th>State</th><th>Actions</th></tr>
      ${vms.map((v) => `
        <tr>
          <td class="tname" style="cursor:pointer" data-vm="${esc(v.name)}">${esc(v.name)}</td>
          <td><span class="state ${v.active ? "on" : ""}">${esc(v.state)}</span></td>
          <td>
            <button class="btn icon" data-start="${esc(v.name)}">${ic.start()}</button>
            <button class="btn icon" data-stop="${esc(v.name)}">${ic.stop()}</button>
          </td>
        </tr>`).join("") || `<tr><td colspan="3" class="empty">No VMs.</td></tr>`}
    </table>`);
    list.querySelectorAll("[data-vm]").forEach((r: Element) => {
      (r as HTMLElement).onclick = () => {
        selectedVm = (r as HTMLElement).dataset.vm!;
        vmTab = "summary";
        renderVm();
      };
    });
    list.querySelectorAll("[data-start]").forEach((b: Element) => {
      (b as HTMLElement).onclick = async (e) => {
        e.stopPropagation();
        const n = (b as HTMLElement).dataset.start!;
        await act(`/proxy/${selectedConn}/domains/${encodeURIComponent(n)}/start`, "POST");
        loadVms();
      };
    });
    list.querySelectorAll("[data-stop]").forEach((b: Element) => {
      (b as HTMLElement).onclick = async (e) => {
        e.stopPropagation();
        const n = (b as HTMLElement).dataset.stop!;
        await act(`/proxy/${selectedConn}/domains/${encodeURIComponent(n)}/shutdown`, "POST");
        loadVms();
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
setHtml(list, `<table class="grid">
      <tr><th>Name</th><th>Bridge</th><th>State</th><th></th></tr>
      ${nets.map((n) => `
        <tr><td>${esc(n.name)}</td><td>${esc(n.bridge)}</td>
        <td><span class="state ${n.active ? "on" : ""}">${n.active ? "active" : "inactive"}</span></td>
        <td><button class="btn danger icon" data-delnet="${esc(n.name)}">${ic.trash()}</button></td></tr>`).join("") || `<tr><td colspan="4" class="empty">No networks.</td></tr>`}
    </table>`);
    list.querySelectorAll("[data-delnet]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => deleteNetwork((b as HTMLElement).dataset.delnet!);
    });
  } catch (e) {
    const list = $("#nets-list");
    if (list) setHtml(list, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

async function loadPools() {
  if (!selectedConn) return;
  try {
    const pools = await api.get<PoolSummary[]>(`/proxy/${selectedConn}/pools`);
    const list = $("#pools-list");
    if (!list) return;
setHtml(list, pools.map((p) => {
      // Allocation vs capacity is the real usage figure libvirt reports.
      const used = p.allocation ?? 0;
      const cap = p.capacity ?? 0;
      const usedPct = cap > 0 ? Math.max(0, Math.min(100, (used / cap) * 100)) : 0;
      return `
      <div class="storage-card ${p.active ? "ok" : ""}">
        <div class="storage-card-head">
          <span class="storage-card-icon">${ic.storage()}</span>
          <span class="storage-card-title">${esc(p.name)}</span>
          <span class="badge ${p.active ? "on" : ""}">${p.active ? "active" : "inactive"}</span>
        </div>
        <div class="metric-value">${usedPct.toFixed(usedPct >= 10 ? 0 : 1)}%</div>
        <div class="meter"><div class="meter-fill ${usedPct > 85 ? "hot" : ""}" style="width:${usedPct.toFixed(1)}%"></div></div>
        <div class="metric-foot">
          <span>${fmtBytes(used)} used</span>
          <span>${fmtBytes(p.available)} free</span>
          <span>${fmtBytes(cap)} total</span>
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
      (b as HTMLElement).onclick = () => loadVolumes((b as HTMLElement).dataset.vols!);
    });
    list.querySelectorAll("[data-upload]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => openUpload((b as HTMLElement).dataset.upload!);
    });
    list.querySelectorAll("[data-delpool]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => deletePool((b as HTMLElement).dataset.delpool!);
    });
  } catch (e) {
    const list = $("#pools-list");
    if (list) setHtml(list, `<div class="empty">${esc((e as Error).message)}</div>`);
  }
}

async function loadVolumes(pool: string) {
  if (!selectedConn) return;
  const target = $(`#vols-${cssId(pool)}`);
  if (!target) return;
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
    <div class="actions">
      <button class="btn" id="vm-start">${ic.start(14)} Start</button>
      <button class="btn" id="vm-shutdown">${ic.stop(14)} Shutdown</button>
      <button class="btn" id="vm-destroy">Force stop</button>
      <button class="btn" id="vm-reboot">${ic.reboot(14)} Reboot</button>
      <button class="btn danger" id="vm-delete">${ic.trash(14)} Delete</button>
    </div>
    <div class="tabbar">
      <button class="tab ${vmTab === "summary" ? "active" : ""}" data-vmtab="summary">${ic.datacenter(14)} Summary</button>
      <button class="tab ${vmTab === "hardware" ? "active" : ""}" data-vmtab="hardware">${ic.cpu(14)} Hardware</button>
      <button class="tab ${vmTab === "console" ? "active" : ""}" data-vmtab="console">${ic.vm(14)} Console</button>
      <button class="tab ${vmTab === "xml" ? "active" : ""}" data-vmtab="xml">XML</button>
    </div>
    <div id="vm-body"></div>`);
  content.querySelectorAll("[data-vmtab]").forEach((b: Element) => {
    (b as HTMLElement).onclick = () => {
      vmTab = (b as HTMLElement).dataset.vmtab as typeof vmTab;
      renderVm();
    };
  });
  $("#vm-start").onclick = () => act(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/start`, "POST").then(renderVm);
  $("#vm-shutdown").onclick = () => act(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/shutdown`, "POST").then(renderVm);
  $("#vm-destroy").onclick = () => act(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/destroy`, "POST").then(renderVm);
  $("#vm-reboot").onclick = () => act(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/reboot`, "POST").then(renderVm);
  $("#vm-delete").onclick = () => deleteVm(vm);
  await renderVmBody(vm);
}

async function renderVmBody(vm: string) {
  if (!selectedConn) return;
  const body = $("#vm-body");
  if (vmTab === "console") {
setHtml(body, `<div class="console-stage" id="console-stage"><div class="console-msg" id="console-msg">connecting…</div></div>`);
    try {
      const g = await api.get<{ graphics: { kind: string } | null }>(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/graphics`);
      const kind = g.graphics?.kind ?? "vnc";
      const url = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/proxy/${selectedConn}/console/domains/${encodeURIComponent(vm)}`;
      attachConsole($("#console-stage"), kind, url, (msg) => {
        const m = $("#console-msg");
        if (m) m.textContent = msg ?? "";
      });
    } catch (e) {
      const m = $("#console-msg");
      if (m) m.textContent = (e as Error).message;
    }
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
        <div class="actions">
          <button class="btn primary" id="hw-edit">${ic.edit(14)} Edit hardware</button>
          ${running ? `<span class="hw-locked">Changes apply after the guest is restarted.</span>` : ""}
        </div>
        <div class="hwtable">${rows}${devRows}</div>
        ${devices.length ? "" : `<div class="hw-note" style="margin-top:12px">No extra devices. Use Edit hardware to add one.</div>`}`);
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
          <div class="chart-axis"><span>-3m</span><span>now</span></div>
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
          <div class="chart-axis"><span>-3m</span><span>now</span></div>
        </div>
        <div class="metric-foot">
          <span>${fmtBytes(usedKib * 1024)} used</span>
          <span>${fmtBytes(s.max_memory_kib * 1024)} max</span>
        </div>
      </div>
    </div>

    <div class="specs">
      <div class="spec"><b>State</b><span>${esc(d.state)}</span></div>
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
    <label class="field"><span>Auth token</span>
      <input id="conn-token" placeholder="${existing ? "leave blank to keep the current token" : "paste existing token or generate"}">
      <label class="check"><input type="checkbox" id="conn-gen"> generate new token</label>
    </label>
    <p class="hint" id="conn-fp">${existing?.fingerprint ? `Pinned certificate: ${esc(existing.fingerprint)}` : ""}</p>`,
    `<button class="btn" id="conn-save">Save</button>`,
  );
  const fpEl = $("#conn-fp", el);
  $("#conn-url", el).onblur = async () => {
    const url = $("#conn-url", el).value.trim();
    if (!url.startsWith("https://")) return;
    try {
      const r = await api.post<{ cert: string; fingerprint: string }>("/connections/_fetch_cert", { url });
      fpEl.textContent = `Certificate fingerprint: ${r.fingerprint}`;
      fpEl.className = "hint ok";
      (el as any)._cert = r.cert;
    } catch (e) {
      fpEl.textContent = `Cert fetch failed: ${(e as Error).message}`;
      fpEl.className = "hint bad";
    }
  };
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
      if ((el as any)._cert) body.cert = (el as any)._cert;

      if (existing) await api.put(`/connections/${existing.id}`, body);
      else await api.post("/connections", body);

      toast(existing ? "Server updated" : "Server added");
      el.remove();
      connections = await api.get<Connection[]>("/connections");
      renderTree();
      renderDc();
      void refreshTreeVms();
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };
}

async function loadFormData(conn: string) {
  const pools = await api.get<PoolSummary[]>(`/proxy/${conn}/pools`);
  const nets = await api.get<NetworkSummary[]>(`/proxy/${conn}/networks`);
  return { pools, nets };
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
          if (f.key === "volume") list = await volumes(d.fields.pool ?? pools[0]?.name ?? "");
          const labels = f.options?.length
            ? f.options.map((o) => `<option value="${esc(o.value)}" ${o.value === val ? "selected" : ""}>${esc(o.label)}</option>`).join("")
            : opt(list, val);
          return `<label class="field"><span>${esc(f.label)}</span>
            <select data-field="${esc(f.key)}">${labels}</select></label>`;
        }
        return `<label class="field"><span>${esc(f.label)}</span>
          <input data-field="${esc(f.key)}" type="${f.type}" value="${esc(val)}"
            placeholder="${esc(f.placeholder ?? "")}"></label>`;
      }));
      return `<div class="hw-page active">
        <h3 class="hw-title">${esc(t.label)}</h3>
        <div class="hw-note">${esc(t.blurb)}</div>
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
          <div class="hw-note">Written to the domain as <code>&lt;vcpu&gt;</code>.</div>
        </div>`;
      case "memory":
        return `<div class="hw-page active">
          <h3 class="hw-title">Memory</h3>
          <label class="field"><span>Memory (MB)</span>
            <input id="g-mem" type="number" min="128" step="256" value="${general.memory_mb}"></label>
          <div class="hw-note">Written as <code>&lt;memory unit="KiB"&gt;</code>.</div>
        </div>`;
      case "disk":
        return `<div class="hw-page active">
          <h3 class="hw-title">Hard Disk</h3>
          <label class="field"><span>Storage pool</span>
            <select id="g-pool">${opt(pools.map((p) => p.name), newDisk.pool)}</select></label>
          <label class="field"><span>Disk size (GB)</span>
            <input id="g-size" type="number" min="1" value="${newDisk.size_gb}"></label>
          <label class="field"><span>Format</span>
            <select id="g-format">${opt(["qcow2", "raw"], newDisk.format)}</select></label>
          <div class="hw-note">A volume is created in the pool, then attached as the boot disk.</div>
        </div>`;
      case "display":
        return `<div class="hw-page active">
          <h3 class="hw-title">Display</h3>
          <label class="field"><span>Graphics</span>
            <select id="g-gfx">${opt(["vnc", "spice"], general.graphics)}</select></label>
          <div class="hw-note">Served through the daemon's console proxy.</div>
        </div>`;
      case "firmware":
        return `<div class="hw-page active">
          <h3 class="hw-title">BIOS</h3>
          <label class="field"><span>Firmware</span>
            <select id="g-fw">${opt(["uefi", "bios"], general.firmware)}</select></label>
          <div class="hw-note">UEFI loads OVMF via <code>&lt;loader&gt;</code>.</div>
        </div>`;
      default:
        return `<div class="hw-page active">
          <h3 class="hw-title">General</h3>
          <label class="field"><span>Name</span>
            <input id="g-name" value="${esc(general.name)}" ${editing ? "disabled" : ""}
              placeholder="arch-box"></label>
          ${editing ? `<div class="hw-note">Renaming an existing domain is not supported here.</div>` : ""}
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
      const fields = await Promise.all(t.fields.map(async (f) => {
        const val = draft[f.key] ?? f.value ?? "";
        if (f.type === "select") {
          let list: string[] = (f.options ?? []).map((o) => o.value);
          if (f.key === "pool") list = pools.map((p) => p.name);
          if (f.key === "network") list = nets.map((n) => n.name);
          if (f.key === "volume") list = await volumes(draft.pool ?? pools[0]?.name ?? "");
          const labels = f.options?.length
            ? f.options.map((o) => `<option value="${esc(o.value)}" ${o.value === val ? "selected" : ""}>${esc(o.label)}</option>`).join("")
            : opt(list, val);
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
          <div class="hw-note">${esc(t.blurb)}</div>
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
      const vm = opts.vm;
      el.remove();
      runTask(`Update ${vm}`, async (set) => {
        set("rebuilding domain definition");
        // Strip the devices this editor owns, then splice the current set back
        // in, so libvirt-added devices we do not model survive the round trip.
        const stripped = baseXml
          .replace(/<disk\b[^>]*device=['"](disk|cdrom)['"][\s\S]*?<\/disk>\s*/g, "")
          .replace(/<interface\b[^>]*type=['"]network['"][\s\S]*?<\/interface>\s*/g, "")
          .replace(/<rng\b[\s\S]*?<\/rng>\s*/g, "")
          .replace(/<memballoon\b[^>]*\/>\s*/g, "")
          .replace(/<channel\b[\s\S]*?org\.qemu\.guest_agent[\s\S]*?<\/channel>\s*/g, "")
          .replace(/<vcpu([^>]*)>\d+<\/vcpu>/, `<vcpu$1>${general.cpus}</vcpu>`)
          .replace(/<memory([^>]*)>\d+<\/memory>/, `<memory$1>${general.memory_mb * 1024}</memory>`)
          .replace(/<currentMemory([^>]*)>\d+<\/currentMemory>/, `<currentMemory$1>${general.memory_mb * 1024}</currentMemory>`);
        set("defining domain");
        await api.put(`/proxy/${conn}/domains/${encodeURIComponent(vm)}/xml`,
          withDevices(stripped, devices));
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
      set("defining domain");
      await api.post(`/proxy/${conn}/domains?start=true`, withDevices(domainXml(form), devices));
    });
    toast("VM creation started");
    renderConn();
  };

  void render();
}

/// Back-compat entry point used by the VMs tab button.
function openCreateVm() {
  void openHardwareEditor();
}

function openCreateNetwork() {
  if (!selectedConn) return;
  const el = openModal(
    "Create network",
    `<div class="grid2">
      <label class="field"><span>Name</span><input id="net-name"></label>
      <label class="field"><span>Mode</span><select id="net-mode">
        <option value="nat">NAT</option><option value="bridge">Bridge</option><option value="none">Isolated</option>
      </select></label>
      <label class="field"><span>Bridge (bridge mode)</span><input id="net-bridge" placeholder="br0"></label>
      <label class="field"><span>Network (CIDR, optional)</span><input id="net-ip" placeholder="192.168.100.1/24"></label>
      <label class="field"><span>DHCP start</span><input id="net-dhcp-start" placeholder="192.168.100.10"></label>
      <label class="field"><span>DHCP end</span><input id="net-dhcp-end" placeholder="192.168.100.200"></label>
    </div>`,
    `<button class="btn primary" id="net-create">Create</button>`,
  );
  $("#net-create", el).onclick = async () => {
    const form: NetworkForm = {
      name: $("#net-name", el).value,
      forward: ($("#net-mode", el) as HTMLSelectElement).value as NetworkForm["forward"],
      bridge: $("#net-bridge", el).value,
      ip: $("#net-ip", el).value,
      dhcp_start: $("#net-dhcp-start", el).value,
      dhcp_end: $("#net-dhcp-end", el).value,
    };
    if (!form.name) return toast("Name is required", "bad");
    el.remove();
    runTask(`Create network ${form.name}`, async () => {
      await api.post(`/proxy/${selectedConn}/networks`, { xml: networkXml(form) });
    });
    toast("Network creation started");
    renderConn();
  };
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
      <label class="field"><span>Target path</span><input id="pool-target" placeholder="/var/lib/libvirt/images"></label>
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
      await api.post(`/proxy/${selectedConn}/pools`, { xml: poolXml(form) });
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
    `<label class="field"><span>Volume name</span><input id="up-name" placeholder="arch.iso"></label>
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

function openProfile() {
  if (!user) return;
  const el = openModal(
    "Profile",
    `<label class="field"><span>Nickname</span><input id="pf-nick" value="${esc(user.nickname)}"></label>
     <label class="field"><span>Avatar</span>
       <input id="pf-avatar" type="file" accept="image/png,image/jpeg,image/webp">
     </label>
     <div class="actions">
       <button class="btn primary" id="pf-save">Save nickname</button>
       <button class="btn" id="pf-upload">Upload avatar</button>
     </div>
     <div class="section-title">API keys</div>
     <div id="pf-keys"></div>
     <div class="actions"><button class="btn" id="pf-newkey">New API key</button></div>`,
    `<button class="btn" id="pf-close">Close</button>`,
  );
  $("#pf-close", el).onclick = () => el.remove();
  $("#pf-save", el).onclick = async () => {
    try {
      user = await api.put<User>("/me", { nickname: $("#pf-nick", el).value });
      toast("Saved");
      renderProfileBtn();
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };
  $("#pf-upload", el).onclick = async () => {
    const file = ($("#pf-avatar", el) as HTMLInputElement).files?.[0];
    if (!file) return;
    const ext = (file.type.split("/")[1] ?? "png").replace("jpeg", "jpg");
    try {
      user = await api.postRaw<User>(`/me/avatar?ext=${ext}`, file, file.type);
      toast("Avatar updated");
      renderProfileBtn();
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };
  const refreshKeys = async () => {
    const keys = await api.get<import("./api").ApiKey[]>("/me/keys");
    const box = $("#pf-keys", el);
setHtml(box, keys.map((k) => `<div class="task-row"><span class="task-dot done"></span><span class="task-msg">${esc(k.name)} · ${esc(k.prefix)}…</span><button class="btn danger icon" data-kid="${k.id}">${ic.trash()}</button></div>`).join("") || `<div class="empty">No keys.</div>`);
    box.querySelectorAll("[data-kid]").forEach((b: Element) => {
      (b as HTMLElement).onclick = async () => {
        await api.del(`/me/keys/${(b as HTMLElement).dataset.kid}`);
        refreshKeys();
      };
    });
  };
  refreshKeys();
  $("#pf-newkey", el).onclick = async () => {
    const name = await askText("New API key", "Key name", "", "ci-runner");
    if (!name) return;
    try {
      const k = await api.post<import("./api").ApiKey>("/me/keys", { name });
      toast(`API key (copy now): ${k.key}`, "ok");
      refreshKeys();
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };
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
    if (selectedConn && !selectedVm) {
      if (connTab === "vms") loadVms();
      else if (connTab === "networks") loadNetworks();
      else loadPools();
    } else if (selectedConn && selectedVm && vmTab === "summary") {
      renderVmBody(selectedVm);
    }
  }, 3000);
}

// ---------------------------------------------------------------- helpers

function cssId(s: string): string {
  return s.replace(/[^a-zA-Z0-9]/g, "_");
}

boot();
