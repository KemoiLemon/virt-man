import { api } from "./api";
import type { Connection, User } from "./api";
import { ic } from "./icons";
import { parsePath } from "./route";
import { esc } from "./xml";
import { state, treeNets, treePools } from "./state";
import { displayName } from "./util";
import { toast } from "./dialogs";
import { hooks } from "./hooks";
import { applyRoute, beginRender, closeDrawer, fastLoad, guestList, renderCurrent, renderView, syncUrl, toggleDrawer } from "./nav";
import { bindSideResize } from "./sidebar";
import { pollUploads } from "./transfers";
import { $, initTheme, setHtml, toggleTheme } from "./dom";
import { stopPendingPoll } from "./connection-dialog";

/// The open view's server is down: keep its header (so it is clear which
/// server and page this is, and the tree still lines up) and show one offline
/// panel in place of the body.
export function renderOffline() {
  if (!state.selectedConn) return;
  beginRender();
  syncUrl();
  const c = state.connections.find((x) => x.id === state.selectedConn);
  const sc = state.selectedConn;
  const title = state.selectedVm ? displayName(guestList(sc), state.selectedVm)
    : state.selectedNet ? displayName(treeNets[sc], state.selectedNet)
    : state.selectedPool ? displayName(treePools[sc], state.selectedPool)
    : c?.name ?? "";
  const content = $("#content");
  setHtml(content, `<div class="dhead"><span class="dhead-ic">${ic.offline(20)}</span><span class="dtitle">${esc(title)}</span>
    ${state.selectedVm || state.selectedNet || state.selectedPool ? `<span class="badge">${esc(c?.name ?? "")}</span>` : ""}</div>
    ${offlinePanel(state.selectedConn)}`);
}

// ---------------------------------------------------------------- boot

export async function boot() {
  initTheme();
  const status = await api.get<{ needs_setup: boolean; auth_disabled?: boolean }>("/auth/status");
  if (status.auth_disabled) {
    state.authDisabled = true;
    state.user = { id: "local", username: "local", nickname: "local", avatar: null, admin: true, privs: [] };
    await loadConnections();
    applyRoute(parsePath(location.pathname, state.connections));
    renderApp();
    return;
  }
  if (status.needs_setup) return renderSetup();
  try {
    const me = await api.get<User>("/auth/me");
    state.user = me;
    await loadConnections();
    applyRoute(parsePath(location.pathname, state.connections));
    renderApp();
  } catch {
    renderLogin();
  }
}

/// Fetch the saved daemon connections. Without this the sidebar tree and the
/// Datacenter table render empty on every load until one is added or removed.
export async function loadConnections() {
  try {
    state.connections = await api.get<Connection[]>("/connections");
  } catch {
    state.connections = [];
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
      state.user = await api.post<User>("/auth/login", { username: $("#lu").value, password: $("#lp").value });
      // boot() loads connections before rendering; the login path must too, or
      // the sidebar tree and Datacenter table render empty until a refresh.
      await loadConnections();
      // Land on the page they asked for before being sent to sign in.
      applyRoute(parsePath(location.pathname, state.connections));
      renderApp();
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };
  $("#lp").onkeydown = (e: KeyboardEvent) => { if (e.key === "Enter") $("#login-btn").click(); };
}

export async function logout() {
  stopPendingPoll();
  await api.post("/auth/logout");
  state.user = null;
  renderLogin();
}

// ---------------------------------------------------------------- app shell

export function renderApp() {
  if (!state.user) return;
  const app = $("#app");
setHtml(app, `
    <div class="topbar">
      <button class="btn icon mobile-only" id="nav-btn" title="Servers and Guests">${ic.menu()}</button>
      <span class="brand">virt-<span>man</span></span>
      <span class="host">web${state.authDisabled ? " · local" : ` · ${esc(state.user.nickname)}`}</span>
      <span class="topbar-quick">
        <button class="btn icon mobile-only" id="palette-btn" title="Commands">${ic.logs()}</button>
        <button class="btn icon" id="theme-btn" title="Toggle Theme">${ic.theme()}</button>
        ${canAdmin() ? `<button class="btn icon" id="admin-btn"></button>` : ""}
        ${state.authDisabled ? "" : `<button class="btn icon" id="logout-btn" title="Sign Out">${ic.logout()}</button>`}
      </span>
    </div>
    <div class="layout">
      <div class="pane list-pane">
        <div class="restree" id="tree"></div>
        <div class="side-resize" id="side-resize"></div>
        ${state.authDisabled ? "" : `<div class="side-profile" id="profile-btn"></div>`}
      </div>
      <div class="pane">
        <div class="detail" id="content"></div>
      </div>
    </div>
    <div class="nav-scrim" id="nav-scrim"></div>`);

  closeDrawer();
  $("#nav-btn").onclick = () => toggleDrawer(true);
  $("#nav-scrim").onclick = closeDrawer;
  $("#palette-btn").onclick = () => hooks.openCommandPalette();
  $("#theme-btn").onclick = toggleTheme;
  bindSideResize($("#side-resize"));
  if ($("#logout-btn")) $("#logout-btn").onclick = logout;
  if ($("#admin-btn")) $("#admin-btn").onclick = () => (state.adminSection ? hooks.closeAdmin() : hooks.openAdmin());

  hooks.renderProfileBtn();
  renderCurrent();
  // Populate the auto-expanded tree immediately rather than waiting for the
  // first poll tick or a server click.
  void fastLoad(() => hooks.refreshTreeVms());
  hooks.startPolling();
  // Pick up a transfer that was already running before this page loaded, so a
  // reload mid-upload still shows its progress instead of looking stalled.
  void pollUploads();
}

/// The account's base role, as the server names it.
///
/// Matches `perm::ADMIN_ROLE` ("Administrator") and `perm::DEFAULT_ROLE`
/// ("User") exactly, capitalization included: those are the names the ACL and
/// the role list use, so the profile showing "administrator" was the odd one
/// out. Written once here because the header renders in two places (the sidebar
/// card and the profile card), and two literals would drift.
export function roleLabel(isAdmin: boolean | undefined): string {
  return isAdmin ? "Administrator" : "User";
}

/// Whether to offer the admin page: `Permissions.Modify` at `/`, as the
/// server reports it in /auth/me. Hiding the button is cosmetic -- the API
/// refuses every admin call from anyone else.
export function canAdmin(): boolean {
  return !!state.user?.admin;
}

// ---------------------------------------------------------------- downtime

/// Whether the web server's probes currently reach this server's daemon.
export function isOffline(connId: string | null): boolean {
  return !!connId && state.connections.find((c) => c.id === connId)?.online === false;
}

/// "3 min", "2 h", for how long something has been down.
export function ago(ms: number): string {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return `${s} s`;
  if (s < 3600) return `${Math.round(s / 60)} min`;
  if (s < 86400) return `${Math.round(s / 3600)} h`;
  return `${Math.round(s / 86400)} d`;
}

/// One panel for a server that cannot be reached, in place of the raw
/// transport error each view used to print ("daemon error: error sending
/// request … Connection refused").
export function offlinePanel(connId: string): string {
  const c = state.connections.find((x) => x.id === connId);
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
export async function refreshHealth() {
  let fresh: Connection[];
  try {
    fresh = await api.get<Connection[]>("/connections");
  } catch {
    return; // the web server itself is unreachable; nothing to compare
  }
  const was = new Map(state.connections.map((c) => [c.id, c.online !== false] as const));
  const flipped = fresh.filter((c) => was.has(c.id) && was.get(c.id) !== (c.online !== false));
  state.connections = fresh;
  if (!flipped.length) return;
  hooks.renderTree();
  for (const c of flipped) {
    toast(c.online === false ? `${c.name} went offline` : `${c.name} is back online`, c.online === false ? "bad" : "ok");
  }
  if (!state.selectedConn || flipped.some((c) => c.id === state.selectedConn)) {
    if (flipped.some((c) => c.online !== false)) void hooks.refreshTreeVms();
    renderView();
  }
}
