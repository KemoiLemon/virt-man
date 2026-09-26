import { api } from "./api";
import type { Connection, DomainSummary, NetworkSummary, PoolSummary, User, VolumeSummary } from "./api";
import { attachConsole, isTauri, tauriWindow } from "./console";
import DOMPurify from "dompurify";
import { allTasks, clearTask, onTasks, runTask } from "./task";
import { domainXml, esc, networkXml, poolXml, volumeXml } from "./xml";
import type { DomainForm, NetworkForm, PoolForm } from "./xml";

/// Single HTML injection point. Every template's dynamic parts are escaped via
/// esc() at the call site; DOMPurify is the sanitizing backstop the XSS rule
/// requires.
function setHtml(el: Element, html: string) {
  // pi-lens-ignore: no-inner-html-js
  el.innerHTML = DOMPurify.sanitize(html);
}

// ---------------------------------------------------------------- state

let user: User | null = null;
let authDisabled = false;
let connections: Connection[] = [];
let selectedConn: string | null = null;
let connTab: "vms" | "networks" | "storage" = "vms";
let selectedVm: string | null = null;
let vmTab: "summary" | "console" | "xml" = "summary";
let dcTab: "servers" | "users" = "servers";
let pollTimer: number | null = null;

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
    renderApp();
    return;
  }
  if (status.needs_setup) return renderSetup();
  try {
    const me = await api.get<User>("/auth/me");
    user = me;
    renderApp();
  } catch {
    renderLogin();
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
      <button class="btn icon" id="theme-btn" title="Toggle theme">◐</button>
      <button class="btn" id="webui-btn">Connect to webui</button>
      ${authDisabled ? "" : `<button class="btn" id="logout-btn">Sign out</button>`}
      ${isTauri() ? `<span style="margin-left:auto;display:flex;gap:6px">
        <button class="btn icon" id="win-min" title="Minimise">─</button>
        <button class="btn icon" id="win-max" title="Maximise">□</button>
        <button class="btn icon danger" id="win-close" title="Close">✕</button>
      </span>` : ""}
    </div>
    <div class="layout">
      <div class="pane list-pane">
        <div class="tree" id="tree"></div>
        <div class="side-profile" id="profile-btn"></div>
      </div>
      <div class="pane">
        <div class="detail" id="content"></div>
      </div>
    </div>
    <div class="tasks hidden" id="tasks">
      <div class="tasks-head"><span>Task Log</span><button class="btn icon" id="tasks-close">✕</button></div>
      <div class="tasks-body" id="tasks-body"></div>
    </div>`);

  $("#theme-btn").onclick = toggleTheme;
  if ($("#logout-btn")) $("#logout-btn").onclick = logout;
  $("#webui-btn").onclick = connectToWebui;
  $("#tasks-close").onclick = () => $("#tasks").classList.add("hidden");

  onTasks(renderTasks);
  renderTree();
  renderProfileBtn();
  wireWindowChrome();
  if (!selectedConn) renderDc();
  startPolling();
}

function wireWindowChrome() {
  // Frameless titlebar on Tauri; browser build has no window chrome.
  const bar = document.querySelector(".topbar");
  if (bar && isTauri()) bar.setAttribute("data-tauri-drag-region", "");
  if (!isTauri()) return;
  $("#win-min")?.addEventListener("click", async () => (await tauriWindow())?.minimize());
  $("#win-max")?.addEventListener("click", async () => {
    const w = await tauriWindow();
    if (!w) return;
    if (await w.isMaximized()) await w.unmaximize();
    else await w.maximize();
  });
  $("#win-close")?.addEventListener("click", async () => (await tauriWindow())?.close());
}

/// Navigate the app window to a remote web UI. Standalone desktop builds use
/// this instead of local connections; the remote web UI is the same software.
async function connectToWebui() {
  const url = localStorage.getItem("virt-man-webui") ?? "https://localhost:8006";
  const next = window.prompt("Web UI URL", url);
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

function renderTree() {
  const tree = $("#tree");
  const connRows = connections
    .map(
      (c) => `
      <div class="tree-row ${c.id === selectedConn ? "sel" : ""}" data-conn="${c.id}">
        <span class="dot"></span>
        <span class="tname">${esc(c.name)}</span>
        <span class="state">daemon</span>
      </div>`,
    )
    .join("");
setHtml(tree, `
    <div class="tree-row node ${selectedConn === null ? "sel" : ""}" id="dc-row">Datacenter</div>
    ${connRows}
    <div class="tree-row node" id="add-conn-row" style="color:var(--accent)">＋ Add Server</div>`);

  $("#dc-row").onclick = () => {
    selectedConn = null;
    selectedVm = null;
    dcTab = "servers";
    renderTree();
    renderDc();
  };
  $("#add-conn-row").onclick = () => openConnectionDialog();
  tree.querySelectorAll("[data-conn]").forEach((row: Element) => {
    (row as HTMLElement).onclick = () => {
      selectedConn = (row as HTMLElement).dataset.conn!;
      selectedVm = null;
      connTab = "vms";
      renderTree();
      renderConn();
    };
  });
}

function renderProfileBtn() {
  const el = $("#profile-btn");
  if (!el) return;
  if (authDisabled) {
    setHtml(el, `<div class="who"><b>local</b><span>desktop</span></div>`);
    el.onclick = null;
    return;
  }
  if (!user) return;
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
  const tabs = dcTab === "servers"
    ? `<div class="actions">
        <button class="btn primary" id="dc-add">Add Server</button>
        ${authDisabled ? "" : `<button class="btn" id="dc-users">Users &amp; API keys</button>`}
      </div>`
    : `<div class="actions"><button class="btn" id="dc-back">Back to servers</button></div>`;
  const body = dcTab === "servers"
    ? `<div class="section-title">Servers</div>
       <table class="grid">
         <tr><th>Name</th><th>URL</th><th>Cert</th><th></th></tr>
         ${connections.map((c) => `
           <tr>
             <td>${esc(c.name)}</td>
             <td>${esc(c.url)}</td>
             <td>${c.fingerprint ? c.fingerprint.slice(0, 17) + "…" : "—"}</td>
             <td>
               <button class="btn icon" data-test="${c.id}">test</button>
               <button class="btn danger icon" data-del="${c.id}">✕</button>
             </td>
           </tr>`).join("") || `<tr><td colspan="4" class="empty">No servers yet — add one.</td></tr>`}
       </table>`
    : `<div id="users-area">loading…</div>`;
setHtml(content, `<div class="dhead"><span class="dtitle">Datacenter</span></div>${tabs}${body}`);

  $("#dc-add")?.addEventListener("click", () => openConnectionDialog());
  $("#dc-users")?.addEventListener("click", () => { dcTab = "users"; renderDc(); });
  $("#dc-back")?.addEventListener("click", () => { dcTab = "servers"; renderDc(); });
  content.querySelectorAll("[data-test]").forEach((b: Element) => {
    (b as HTMLElement).onclick = () => testConnection((b as HTMLElement).dataset.test!);
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
          <td><button class="btn danger icon" data-deluser="${u.id}">✕</button></td></tr>`).join("")}
      </table>
      <div class="actions"><button class="btn primary" id="add-user">Add user</button></div>
      <div class="section-title">API keys</div>
      <table class="grid">
        <tr><th>Name</th><th>Prefix</th><th></th></tr>
        ${keys.map((k) => `
          <tr><td>${esc(k.name)}</td><td>${esc(k.prefix)}…</td>
          <td><button class="btn danger icon" data-delkey="${k.id}">✕</button></td></tr>`).join("") || `<tr><td colspan="3" class="empty">No keys.</td></tr>`}
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
      <span class="actions" style="margin:0;margin-left:auto">
        <button class="btn" data-tab="vms">VMs</button>
        <button class="btn" data-tab="networks">Networks</button>
        <button class="btn" data-tab="storage">Storage</button>
      </span>
    </div>
    <div class="dsub">${esc(conn.url)}${conn.fingerprint ? " · " + conn.fingerprint : ""}</div>
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
  if (connTab === "vms") {
setHtml(body, `<div class="actions"><button class="btn primary" id="create-vm">Create VM</button></div><div id="vms-list">loading…</div>`);
    $("#create-vm").onclick = () => openCreateVm();
    await loadVms();
  } else if (connTab === "networks") {
setHtml(body, `<div class="actions"><button class="btn primary" id="create-net">Create network</button></div><div id="nets-list">loading…</div>`);
    $("#create-net").onclick = () => openCreateNetwork();
    await loadNetworks();
  } else {
setHtml(body, `<div class="actions"><button class="btn primary" id="create-pool">Create pool</button></div><div id="pools-list">loading…</div>`);
    $("#create-pool").onclick = () => openCreatePool();
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
            <button class="btn icon" data-start="${esc(v.name)}">▶</button>
            <button class="btn icon" data-stop="${esc(v.name)}">■</button>
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
        <td><button class="btn danger icon" data-delnet="${esc(n.name)}">✕</button></td></tr>`).join("") || `<tr><td colspan="4" class="empty">No networks.</td></tr>`}
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
setHtml(list, pools.map((p) => `
      <div class="specs" style="margin-bottom:8px">
        <div class="spec"><b>Pool</b><span>${esc(p.name)} ${p.active ? "●" : "○"}</span></div>
        <div class="spec"><b>Capacity</b><span>${fmtBytes(p.capacity)}</span></div>
        <div class="spec"><b>Available</b><span>${fmtBytes(p.available)}</span></div>
      </div>
      <div class="actions" style="margin-bottom:12px">
        <button class="btn" data-vols="${esc(p.name)}">Volumes</button>
        <button class="btn" data-upload="${esc(p.name)}">Upload ISO</button>
        <button class="btn danger icon" data-delpool="${esc(p.name)}">✕</button>
      </div>
      <div id="vols-${cssId(p.name)}" style="margin-bottom:14px"></div>`).join("") || `<div class="empty">No pools.</div>`);
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
        <td><button class="btn danger icon" data-delvol="${esc(v.name)}">✕</button></td></tr>`).join("") || `<tr><td colspan="4" class="empty">No volumes.</td></tr>`}
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
    <div class="dhead"><span class="dtitle">${esc(vm)}</span>
      <span class="actions" style="margin:0;margin-left:auto">
        <button class="btn" data-vmtab="summary">Summary</button>
        <button class="btn" data-vmtab="console">Console</button>
        <button class="btn" data-vmtab="xml">XML</button>
      </span>
    </div>
    <div class="actions" style="margin-top:10px">
      <button class="btn" id="vm-start">Start</button>
      <button class="btn" id="vm-shutdown">Shutdown</button>
      <button class="btn" id="vm-destroy">Force stop</button>
      <button class="btn" id="vm-reboot">Reboot</button>
      <button class="btn danger" id="vm-delete">Delete</button>
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
  if (vmTab === "xml") {
    try {
      const { xml } = await api.get<{ xml: string }>(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/xml`);
setHtml(body, `<div class="cmd">${esc(xml)}</div>`);
    } catch (e) {
setHtml(body, `<div class="empty">${esc((e as Error).message)}</div>`);
    }
    return;
  }
  // summary
  try {
    const d = await api.get<any>(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}`);
    const g = await api.get<{ graphics: { kind: string; host: string; port: number } | null }>(`/proxy/${selectedConn}/domains/${encodeURIComponent(vm)}/graphics`);
setHtml(body, `<div class="specs">
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
  if (!confirm(`Delete VM "${name}"?`)) return;
  const removeStorage = confirm("Also remove its storage?");
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
  if (!selectedConn || !confirm(`Delete network "${name}"?`)) return;
  try {
    await api.del(`/proxy/${selectedConn}/networks/${encodeURIComponent(name)}`);
    toast("Deleted");
    loadNetworks();
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}

async function deletePool(name: string) {
  if (!selectedConn || !confirm(`Delete pool "${name}"?`)) return;
  try {
    await api.del(`/proxy/${selectedConn}/pools/${encodeURIComponent(name)}`);
    toast("Deleted");
    loadPools();
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}

async function deleteVolume(pool: string, name: string) {
  if (!selectedConn || !confirm(`Delete volume "${name}"?`)) return;
  try {
    await api.del(`/proxy/${selectedConn}/pools/${encodeURIComponent(pool)}/volumes/${encodeURIComponent(name)}`);
    toast("Deleted");
    loadVolumes(pool);
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}

async function testConnection(id: string) {
  try {
    const r = await api.get<{ ok: boolean; status: number; body: string }>(`/connections/${id}/test`);
    toast(r.ok ? `Daemon OK (${r.status})` : `Daemon replied ${r.status}: ${r.body}`, r.ok ? "ok" : "bad");
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}

async function deleteConnection(id: string) {
  const c = connections.find((x) => x.id === id);
  if (!c || !confirm(`Remove server "${c.name}"?`)) return;
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
      <div class="modal-head"><span>${title}</span><button class="x">✕</button></div>
      <div class="modal-body">${body}</div>
      <div class="modal-foot">${foot}</div>
    </div>`);
  document.body.appendChild(el);
  (el.querySelector(".x") as HTMLElement).onclick = () => el.remove();
  el.onclick = (e) => { if (e.target === el) el.remove(); };
  return el;
}

function openConnectionDialog() {
  const el = openModal(
    "Add Server",
    `
    <label class="field"><span>Name</span><input id="conn-name"></label>
    <label class="field"><span>URL (https://host:8443)</span><input id="conn-url" placeholder="https://host:8443"></label>
    <label class="field"><span>Auth token</span>
      <input id="conn-token" placeholder="paste existing token or generate">
      <label class="check"><input type="checkbox" id="conn-gen"> generate new token</label>
    </label>
    <p class="hint" id="conn-fp"></p>`,
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
      if (!body.generate_token) body.token = $("#conn-token", el).value;
      if ((el as any)._cert) body.cert = (el as any)._cert;
      await api.post("/connections", body);
      toast("Server added");
      el.remove();
      connections = await api.get<Connection[]>("/connections");
      renderTree();
      renderDc();
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

function poolOptions(pools: PoolSummary[]) {
  return pools.map((p) => `<option value="${esc(p.name)}">${esc(p.name)}</option>`).join("");
}

function openCreateVm() {
  if (!selectedConn) return;
  const el = openModal(
    "Create VM",
    `<div class="grid2">
      <label class="field"><span>Name</span><input id="vm-name"></label>
      <label class="field"><span>vCPUs</span><input id="vm-cpus" type="number" value="2" min="1"></label>
      <label class="field"><span>Memory (MB)</span><input id="vm-mem" type="number" value="2048" min="128"></label>
      <label class="field"><span>Firmware</span><select id="vm-fw"><option>uefi</option><option>bios</option></select></label>
      <label class="field"><span>Graphics</span><select id="vm-gfx"><option>vnc</option><option>spice</option></select></label>
      <label class="field"><span>Network</span><select id="vm-net"><option value="">none</option></select></label>
      <label class="field"><span>Storage pool</span><select id="vm-pool"></select></label>
      <label class="field"><span>Disk</span>
        <select id="vm-disk-mode">
          <option value="new">New disk</option>
          <option value="existing">Existing volume</option>
        </select>
      </label>
      <label class="field" id="vm-new-size-wrap"><span>Disk size (GB)</span><input id="vm-disk-size" type="number" value="10" min="1"></label>
      <label class="field hidden" id="vm-existing-wrap"><span>Volume</span><select id="vm-existing"></select></label>
      <label class="field"><span>Disk format</span><select id="vm-format"><option>qcow2</option><option>raw</option></select></label>
      <label class="field"><span>ISO (optional)</span><select id="vm-iso"><option value="">none</option></select></label>
    </div>
    <p class="hint" id="vm-xml-preview"></p>`,
    `<button class="btn primary" id="vm-create">Create</button>`,
  );
  loadFormData(selectedConn).then(async ({ pools, nets }) => {
setHtml($("#vm-pool", el), poolOptions(pools));
setHtml($("#vm-net", el), `<option value="">none</option>` + nets.map((n) => `<option>${esc(n.name)}</option>`).join(""));
    const vols = async (pool: string) => {
      const v = await api.get<VolumeSummary[]>(`/proxy/${selectedConn}/pools/${encodeURIComponent(pool)}/volumes`);
      const opts = v.map((x) => `<option>${esc(x.name)}</option>`).join("");
setHtml($("#vm-existing", el), opts);
setHtml($("#vm-iso", el), `<option value="">none</option>` + opts);
    };
    const firstPool = pools[0]?.name;
    if (firstPool) await vols(firstPool);
    $("#vm-pool", el).onchange = () => vols(($("#vm-pool", el) as HTMLSelectElement).value);
    $("#vm-disk-mode", el).onchange = () => {
      const mode = ($("#vm-disk-mode", el) as HTMLSelectElement).value;
      $("#vm-new-size-wrap", el).classList.toggle("hidden", mode !== "new");
      $("#vm-existing-wrap", el).classList.toggle("hidden", mode !== "existing");
    };
    $("#vm-create", el).onclick = async () => {
      const pool = ($("#vm-pool", el) as HTMLSelectElement).value;
      const form: DomainForm = {
        name: $("#vm-name", el).value,
        cpus: +$("#vm-cpus", el).value,
        memory_mb: +$("#vm-mem", el).value,
        firmware: ($("#vm-fw", el) as HTMLSelectElement).value as "uefi" | "bios",
        graphics: ($("#vm-gfx", el) as HTMLSelectElement).value as "vnc" | "spice",
        network: ($("#vm-net", el) as HTMLSelectElement).value,
        bridge: "",
        pool,
        new_disk: ($("#vm-disk-mode", el) as HTMLSelectElement).value === "new",
        existing_volume: ($("#vm-existing", el) as HTMLSelectElement).value,
        disk_size_gb: +$("#vm-disk-size", el).value,
        disk_format: ($("#vm-format", el) as HTMLSelectElement).value as "raw" | "qcow2",
        iso: ($("#vm-iso", el) as HTMLSelectElement).value,
        iso_pool: pool,
      };
      if (!form.name) return toast("Name is required", "bad");
      el.remove();
      runTask(`Create VM ${form.name}`, async (set) => {
        set("defining domain");
        const xml = domainXml(form);
        if (form.new_disk) {
          set(`creating ${form.disk_size_gb} GB volume`);
          await api.post(`/proxy/${selectedConn}/pools/${encodeURIComponent(pool)}/volumes`, {
            xml: volumeXml({ name: `${form.name}.qcow2`, capacity_gb: form.disk_size_gb, format: form.disk_format }),
          });
          form.new_disk = false;
          form.existing_volume = `${form.name}.qcow2`;
        }
        await api.post(`/proxy/${selectedConn}/domains?start=true`, xml);
      });
      toast("VM creation started");
      renderConn();
    };
  });
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
setHtml(box, keys.map((k) => `<div class="task-row"><span class="task-dot done"></span><span class="task-msg">${esc(k.name)} · ${esc(k.prefix)}…</span><button class="btn danger icon" data-kid="${k.id}">✕</button></div>`).join("") || `<div class="empty">No keys.</div>`);
    box.querySelectorAll("[data-kid]").forEach((b: Element) => {
      (b as HTMLElement).onclick = async () => {
        await api.del(`/me/keys/${(b as HTMLElement).dataset.kid}`);
        refreshKeys();
      };
    });
  };
  refreshKeys();
  $("#pf-newkey", el).onclick = async () => {
    const name = prompt("Key name");
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
      <button class="btn icon" data-clear="${t.id}">✕</button>
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

function fmtBytes(n: number | null | undefined): string {
  if (n == null) return "—";
  if (n === 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)} ${units[i]}`;
}

function cssId(s: string): string {
  return s.replace(/[^a-zA-Z0-9]/g, "_");
}

boot();
