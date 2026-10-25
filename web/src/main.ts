import { api, pendingReads, pollWindow } from "./api";
import type { ContainerSummary, DomainSummary, FirewallForward, FirewallPolicy, FirewallRule, NetworkSummary, PoolSummary, VolumeSummary } from "./api";
import { attachConsole } from "./console";
import { closeTayne, tayneOpen } from "./tayne";
import { ic } from "./icons";
import { skeleton } from "./skeleton";
import { holdFocus } from "./focus";
import { isCoarsePointer } from "./touch";
import { BOOT_LABEL, bootChoices, bootTargets, deviceSummary, devicesFromXml, hwType } from "./hardware";
import { poolUsed } from "./pool-usage";
import { addDiskSample, addHostSample, addNetSample, addSample, fmtBytes, fmtRate, pct, sparkline } from "./stats";
import type { Timing } from "./stats";
import { isLive, loadHistory, range, rangeSelect } from "./history";
import { DIR_LABEL, GROUP_LABEL, KIND_LABEL, PAGE, defaultFilter, drillDown, fmtWhen, forLive, liveApplies, logQuery, sortMark, toggleSort, withGroup } from "./access";
import type { FlowDir, Group, Kind, LogFilter } from "./access";
import type { DomainStats, HostStats } from "./stats";
import { runTask } from "./task";
import { initIndicators } from "./indicator";
import { closeContextMenu, contextMenuOpen } from "./ctxmenu";
import { attachSuggest } from "./suggest";
import { esc, nextFreeIp } from "./xml";

import { shownNet, state, summaryDiskSel, summaryNetSel, treeNets, treePools, treeVms } from "./state";
import { bridgeNetwork, capLabel, displayName, emptyState, nicNetwork, unescapeXml } from "./util";

import { askConfirm, askMultiline, openModal, toast } from "./dialogs";
import { act, deleteVm } from "./actions";
import { hooks } from "./hooks";
import { beginRender, clearResourceSelection, closeConsole, fastLoad, guestBase, guestList, leaveCurrentView, stillCurrentVm, syncUrl } from "./nav";
import { profileHooks } from "./profile";
import { transferHooks } from "./transfers";
import { $, setHtml } from "./dom";
import { openHardwareEditor } from "./hardware-editor";
import { openCreateVm } from "./create-vm";
import { dcCreate, openCreatePool, openCreateResource } from "./create-resource";
import { openCreateNetwork } from "./network-form";
import { boot, canAdmin, isOffline, refreshHealth, renderOffline, roleLabel } from "./boot";
import { closeAdmin, openAdmin, showSecret } from "./admin-window";
import { renderProfileBtn } from "./menus"; // also installs the contextmenu/long-press listeners
import { openCommandPalette } from "./palette";
import { canShareVm, downloadVv, openShareVm, refreshTreeVms, renderTree, selectCtRow, selectNetRow, selectPoolRow, selectVmRow } from "./tree";
import { loadDcDashboard, renderDc } from "./datacenter";
import { loadNetworks, renderNetPage } from "./network-page";
import { loadPools, loadVolumes, renderPoolPage } from "./pool-page";
import { afterCtDelete, deleteCt, deleteCtOn, loadCts, openCreateCt, openTemplates, renameCt, renderCtHardware, renderCtOptions } from "./containers";
import { bindDashOpeners } from "./resource-common";
import type { IpamHost, IpamInfo } from "./resource-common";

// Moved modules cannot import the views (a cycle), so they reach them through
// `hooks`. Wrappers, so nothing here depends on declaration order.
hooks.renderTree = () => renderTree();
hooks.renderOffline = () => renderOffline();
hooks.isOffline = (conn) => isOffline(conn);
hooks.canAdmin = () => canAdmin();
hooks.openAdmin = (...a) => openAdmin(...(a as Parameters<typeof openAdmin>));
hooks.closeAdmin = () => closeAdmin();
hooks.renderDc = () => renderDc();
hooks.loadDcDashboard = () => loadDcDashboard();
hooks.renderConn = () => renderConn();
hooks.loadDashboard = () => loadDashboard();
hooks.renderVm = () => renderVm();
hooks.renderVmBody = (vm: string) => renderVmBody(vm);
hooks.renderNetPage = () => renderNetPage();
hooks.renderPoolPage = () => renderPoolPage();
hooks.refreshTreeVms = () => refreshTreeVms();
hooks.loadNetworks = () => loadNetworks();
hooks.loadPools = () => loadPools();
hooks.loadVolumes = (pool: string) => loadVolumes(pool);
hooks.deleteCtOn = (...a) => deleteCtOn(...(a as Parameters<typeof deleteCtOn>));
hooks.dcCreate = (...a) => dcCreate(...(a as Parameters<typeof dcCreate>));
hooks.openCreateCt = (...a) => openCreateCt(...(a as Parameters<typeof openCreateCt>));
hooks.openCommandPalette = () => openCommandPalette();
hooks.renderProfileBtn = () => renderProfileBtn();
hooks.startPolling = () => startPolling();
hooks.syncVmActions = () => syncVmActions();
hooks.renameCt = (...a) => renameCt(...(a as Parameters<typeof renameCt>));
hooks.deleteCt = (...a) => deleteCt(...(a as Parameters<typeof deleteCt>));
hooks.afterCtDelete = (...a) => afterCtDelete(...(a as Parameters<typeof afterCtDelete>));
hooks.openFirewallEditor = (...a) => openFirewallEditor(...(a as Parameters<typeof openFirewallEditor>));
hooks.openTemplates = (...a) => openTemplates(...(a as Parameters<typeof openTemplates>));
hooks.selectVmRow = (...a) => selectVmRow(...(a as Parameters<typeof selectVmRow>));
hooks.selectCtRow = (...a) => selectCtRow(...(a as Parameters<typeof selectCtRow>));
hooks.selectNetRow = (...a) => selectNetRow(...(a as Parameters<typeof selectNetRow>));
hooks.selectPoolRow = (...a) => selectPoolRow(...(a as Parameters<typeof selectPoolRow>));
// transfers.ts repaints the pool page through its own hook.
transferHooks.renderPoolPage = renderPoolPage;
// Likewise profile.ts: wrappers, so nothing here depends on declaration order.
profileHooks.renderProfileBtn = () => renderProfileBtn();
profileHooks.roleLabel = (admin) => roleLabel(admin);
profileHooks.showSecret = (title, secret) => showSecret(title, secret);

// ------------------------------------------------------------- global keys

/// Ctrl+K opens the palette; Escape closes the palette, then the topmost
/// modal (dispatched as a synthetic event so askText/askConfirm can resolve
/// their promise). The guest owns the keyboard while the console is focused,
/// so the palette stays out of the way there.
document.addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && (e.key === "k" || e.key === "K")) {
    // Only inside the real app: the login/setup screens have no commands to
    // offer, and the guest owns the keyboard while the console is focused.
    if (!state.user) return;
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

// ---------------------------------------------------------------- connection view

async function renderConn() {
  if (!state.selectedConn) return;
  if (isOffline(state.selectedConn)) return renderOffline();
  const conn = state.connections.find((c) => c.id === state.selectedConn);
  if (!conn) return;
  // Switching away from the Shell tab must close its session. Most navigation
  // goes through leaveCurrentView; the tab-bar handler re-renders directly, so
  // this is the funnel that covers it.
  if (state.connTab !== "shell") state.terminal?.detachTextConsole();
  // Server view owns the pane: invalidates any in-flight resource render.
  beginRender();
  syncUrl();
  const content = $("#content");
  const restoreTabFocus = holdFocus(content, ".tab.active");
setHtml(content, `
    <div class="dhead"><span class="dhead-ic">${ic.server(20)}</span><span class="dtitle">${esc(conn.name)}</span></div>
    <div class="tabbar">
      <button class="tab ${state.connTab === "dashboard" ? "active" : ""}" data-tab="dashboard">${ic.datacenter(14)} Dashboard</button>
      <button class="tab ${state.connTab === "vms" ? "active" : ""}" data-tab="vms">${ic.vms(14)} VMs</button>
      <button class="tab ${state.connTab === "containers" ? "active" : ""}" data-tab="containers">${ic.container(14)} Containers</button>
      <button class="tab ${state.connTab === "networks" ? "active" : ""}" data-tab="networks">${ic.network(14)} Networks</button>
      <button class="tab ${state.connTab === "storage" ? "active" : ""}" data-tab="storage">${ic.storage(14)} Storage</button>
      <button class="tab ${state.connTab === "firewall" ? "active" : ""}" data-tab="firewall">${ic.firewall(14)} Firewall</button>
      ${canAdmin() ? `<button class="tab ${state.connTab === "shell" ? "active" : ""}" data-tab="shell">${ic.terminal(14)} Shell</button>` : ""}
      <div class="tabbar-actions">
        ${state.connTab === "dashboard" ? `<button class="btn primary" id="create-resource">${ic.plus(14)} Create Resource</button>` : ""}
        ${state.connTab === "vms" ? `<button class="btn primary" id="create-vm">${ic.plus(14)} Create VM</button>` : ""}
        ${state.connTab === "containers" ? `<button class="btn primary" id="create-ct">${ic.plus(14)} Create CT</button>` : ""}
        ${state.connTab === "networks" ? `<button class="btn primary" id="create-net">${ic.plus(14)} Create Network</button>` : ""}
        ${state.connTab === "storage" ? `<button class="btn primary" id="create-pool">${ic.plus(14)} Create Pool</button>` : ""}
      </div>
    </div>
    <div id="conn-body"></div>`);
  restoreTabFocus();
  content.querySelectorAll("[data-tab]").forEach((b: Element) => {
    (b as HTMLElement).onclick = () => {
      state.connTab = (b as HTMLElement).dataset.tab as typeof state.connTab;
      // Clear the whole resource selection, not just state.selectedVm: leaving
      // state.selectedNet/state.selectedPool set made the 1s poll re-render that network
      // or pool page about a second after the user switched to a server tab.
      clearResourceSelection();
      renderConn();
    };
  });
  renderConnBody();
  $("#create-resource")?.addEventListener("click", () => openCreateResource());
  $("#create-vm")?.addEventListener("click", () => openCreateVm());
  $("#create-ct")?.addEventListener("click", () => void openCreateCt());
  $("#create-net")?.addEventListener("click", () => openCreateNetwork());
  $("#create-pool")?.addEventListener("click", () => openCreatePool());
}

async function renderConnBody() {
  if (!state.selectedConn) return;
  const body = $("#conn-body");
  // The create buttons live on the tab bar (rendered by renderConn), so the
  // body only carries the list itself.
  if (state.connTab === "shell") {
    // Re-rendering replaces the whole body, stage included; drop any live
    // session first, or its read loop outlives the element it painted into.
    state.terminal?.detachTextConsole();
    renderShellTab(body, state.selectedConn);
  } else if (state.connTab === "dashboard") {
    setHtml(body, `<div id="dash">${skeleton("dashboard")}</div>`);
    await loadDashboard();
  } else if (state.connTab === "vms") {
    setHtml(body, `<div id="vms-list" class="card-grid">${skeleton("cards")}</div>`);
    await fastLoad(loadVms);
  } else if (state.connTab === "containers") {
    setHtml(body, `<div id="cts-list">${skeleton("cards")}</div>`);
    await fastLoad(loadCts);
  } else if (state.connTab === "networks") {
    setHtml(body, `<div id="nets-list" class="card-grid">${skeleton("cards")}</div>`);
    await fastLoad(loadNetworks);
  } else if (state.connTab === "storage") {
    setHtml(body, `<div id="pools-list">${skeleton("cards")}</div>`);
    state.renderedPools = null; // fresh container: force a full render on first load
    await loadPools();
  } else {
    setHtml(body, `<div id="filters-list">${skeleton("table")}</div>`);
    await fastLoad(loadFilters);
  }
}

/// Daemon shell: a login shell on the server's host, reached through the
/// daemon. The daemon runs the host's own `login(1)` in the pty, so the
/// terminal is the login UI — it shows `login:` and `Password:` itself, and
/// the password never passes through the app.
function renderShellTab(body: Element, conn: string) {
  // The terminal is the whole pane; waiting for the session is its own line.
  setHtml(body, `
    <div class="console-stage term-stage" id="shell-stage" tabindex="0">
      <div class="console-msg" id="shell-msg">connecting…</div>
    </div>`);
  const msg = (m: string | null) => {
    const el = $("#shell-msg");
    if (el) {
      el.textContent = m ?? "";
      el.classList.toggle("hidden", !m);
    }
  };
  void (async () => {
    state.terminal ??= await import("./terminal");
    await state.terminal.attachDaemonShell($("#shell-stage"), `/proxy/${conn}/shell`, msg);
  })();
}

/// Daemon dashboard: host CPU/RAM with sparklines, then counts and summaries
/// for VMs, networks, and storage pools. Reuses the existing list endpoints.
async function loadDashboard() {
  if (!state.selectedConn) return;
  const body = $("#dash");
  if (!body) return;
  try {
    const [h, nets, pools] = await Promise.all([
      api.get<HostStats>(`/proxy/${state.selectedConn}/host/stats`),
      api.get<NetworkSummary[]>(`/proxy/${state.selectedConn}/networks`),
      api.get<PoolSummary[]>(`/proxy/${state.selectedConn}/pools`),
    ]);
    const vms = treeVms[state.selectedConn] ?? [];
    const running = vms.filter((v) => v.active).length;
    const conn = state.selectedConn;
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
      if (range().key !== shown || state.selectedConn !== conn || !$("#dash")) return;
    }
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
          ${sparkline(cpu, "", 100, timing)}
          <div class="chart-axis">${rangeSelect()}<span>now</span></div>
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
          <div class="chart-axis">${rangeSelect()}<span>now</span></div>
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
        <tr><th>Name</th><th>Subnet</th><th>State</th></tr>
        ${nets.map((n) => `
          <tr class="row-open" tabindex="0" data-dash-net="${esc(n.name)}" data-dash-conn="${state.selectedConn}"><td>${esc(n.title || n.name)}</td><td>${n.ip ? esc(n.ip) : "—"}</td>
          <td><span class="state ${n.active ? "on" : ""}">${n.active ? "Active" : "Inactive"}</span></td></tr>`).join("") || `<tr><td colspan="3" class="empty">No networks.</td></tr>`}
      </table>
    </div>

    <div class="dash-section">
      <div class="section-title">Storage Pools</div>
      ${pools.map((p) => {
        const used = poolUsed(p);
        const cap = p.capacity ?? 0;
        const usedPct = cap > 0 ? Math.max(0, Math.min(100, (used / cap) * 100)) : 0;
        return `
        <div class="storage-card ${p.active ? "ok" : ""}" data-open tabindex="0" data-dash-pool="${esc(p.name)}" data-dash-conn="${state.selectedConn}">
          <div class="storage-card-head">
            <span class="storage-card-icon">${ic.storage()}</span>
            <span class="storage-card-title">${esc(p.title || p.name)}</span>
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

/// Per-network firewall editor: rules and NAT port forwards, enforced by the
/// daemon via nftables. Replaces the old host-wide nwfilter preset list.
async function loadFilters() {
  if (!state.selectedConn) return;
  const list = $("#filters-list");
  if (!list) return;
  try {
    const nets = await api.get<NetworkSummary[]>(`/proxy/${state.selectedConn}/networks`);
    setHtml(list, `
      <table class="grid">
        <tr><th>Network</th><th>Type</th><th>Subnet</th><th class="col-actions"></th></tr>
        ${nets.map((n) => `
          <tr>
            <td class="tname">${esc(n.title || n.name)}</td>
            <td>${esc(n.forward === "bridge" ? "Bridge" : n.forward === "nat" ? "NAT" : "Isolated")}</td>
            <td>${n.ip ? esc(n.ip) : "—"}</td>
            <td class="col-actions">
              <div class="row-actions">
                <button class="btn" data-fwedit="${esc(n.name)}">${ic.firewall(14)} Rules</button>
              </div>
            </td>
          </tr>`).join("") || `<tr><td colspan="4" class="empty">No networks.</td></tr>`}
      </table>
      <div class="actions"><button class="btn al-open" id="fw-tab-logs">${ic.logs(14)} Access Logs</button></div>`);
    const conn = state.selectedConn;
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
function openAccessLogs(conn: string, vm = "", ct = false) {
  let f: LogFilter = defaultFilter(vm);
  let vms: string[] = [];
  const el = openModal(`Access Logs${vm ? ` — ${esc(vm)}` : ""}`, `<div id="al-body">${skeleton("table")}</div>`, "");
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
      // Same rule as the 1s poll: never rebuild the table mid-scroll.
      if (busyScrolling()) return;
      void load();
    }, 2000);
  };

  const load = async () => {
    const mine = ++gen;
    try {
      // A container's own log is checked against its container permission
      // (CT.Audit), not VM.Audit; the rows are the same.
      const ctq = ct && vm && f.vm === vm ? "&ct=true" : "";
      const r = await api.get<typeof data>(`/access-logs?${logQuery(conn, f, Date.now())}${ctq}`);
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
          <button class="btn icon" id="al-prev" ${f.page ? "" : "disabled"} title="Newer Page">${ic.back()}</button>
          <span>${f.page + 1} / ${pages}</span>
          <button class="btn icon" id="al-next" ${f.page + 1 < pages ? "" : "disabled"} title="Older Page">${ic.forward()}</button>
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

function fwFwdRow(f: FirewallForward, hosts: IpamHost[], label: (h: IpamHost) => string): string {
  // One port each side, not a range: DNAT maps a port to a port, and a range
  // had no single internal counterpart.
  //
  // The internal address is picked from the guests this network hands one to,
  // so a forward names the guest rather than an address someone has to look up.
  // An address no allocation knows (a forward saved before its guest got one,
  // or a host outside the app) stays selectable: dropping it would silently
  // point the forward at whichever guest happened to be first.
  const known = hosts.some((h) => h.ip === f.lan_ip);
  const current = f.lan_ip && !known
    ? `<option value="${esc(f.lan_ip)}" selected>${esc(f.lan_ip)} — not a guest</option>`
    : "";
  const placeholder = f.lan_ip ? "" : `<option value="" selected>Select a Guest…</option>`;
  return `
    <tr>
      <td><input data-f-wanip placeholder="0.0.0.0 = any" value="${f.wan_ip === "0.0.0.0" ? "" : esc(f.wan_ip)}"></td>
      <td><select data-f-proto>${fwOpt(["tcp", "udp"], f.proto)}</select></td>
      <td><input data-f-wport type="number" min="1" max="65535" value="${f.wan_port}"></td>
      <td><select data-f-lanip>
        ${placeholder}${current}
        ${hosts.map((h) => `<option value="${esc(h.ip)}" ${h.ip === f.lan_ip ? "selected" : ""}>${esc(label(h))} — ${esc(h.ip)}</option>`).join("")}
      </select></td>
      <td><input data-f-lport type="number" min="1" max="65535" value="${f.lan_port}"></td>
      <td><button class="btn danger icon" data-f-del title="Remove">${ic.trash()}</button></td>
    </tr>`;
}

function blankRule(): FirewallRule {
  return { direction: "in", action: "accept", proto: "any", src_ip: "", dst_ip: "", src_port: "", dst_port: "" };
}

function blankPolicy(): FirewallPolicy {
  return { in: "accept", out: "accept", forward: "accept" };
}

/// Boot devices a guest may try, and what to call them. The order they are
/// tried in is the document order of the `<os>` `<boot dev=.../>` elements,
/// which is how libvirt reads them. Defined in `hardware.ts` beside the code
/// that resolves them, so the dropdown and the list cannot disagree.

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
    const num = (sel: string) => {
      const v = +((tr.querySelector(sel) as HTMLInputElement)?.value ?? "");
      return Number.isFinite(v) ? v : 1;
    };
    return {
      proto: (tr.querySelector("[data-f-proto]") as HTMLSelectElement).value as FirewallForward["proto"],
      wan_ip: (tr.querySelector("[data-f-wanip]") as HTMLInputElement).value || "0.0.0.0",
      wan_port: num("[data-f-wport]"),
      lan_ip: (tr.querySelector("[data-f-lanip]") as HTMLSelectElement).value,
      lan_port: num("[data-f-lport]"),
    };
  });
}

/// Network firewall editor: rules and NAT port forwards, in a wide modal.
function openFirewallEditor(netName: string) {
  if (!state.selectedConn) return;
  const conn = state.selectedConn;
  let rules: FirewallRule[] = [];
  let forwards: FirewallForward[] = [];
  let policy: FirewallPolicy = blankPolicy();
  // The addresses this network hands out, with the guest each belongs to, so a
  // forward picks a guest from a list instead of an IP typed from memory.
  let lanHosts: IpamHost[] = [];
  const lanLabel = (h: IpamHost) =>
    (h.vm_name ? displayName(treeVms[conn], h.vm_name) : "") || h.name || h.mac;
  // Every address this daemon knows about, for the source/dest fields.
  // Filled after the modal paints; the suggesters read it lazily per keystroke.
  let ipCandidates: string[] = [];
  void collectIpCandidates(conn).then((ips) => { ipCandidates = ips; });

  const el = openModal(
    `Firewall — ${esc(shownNet(netName))}`,
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
      <div class="section-title fw-cat"><span class="cat-ic">${ic.firewall()}</span>Default Policies</div>
      ${fwPolicyRow(policy)}

      <div class="section-title fw-cat"><span class="cat-ic">${ic.options()}</span>Rules</div>
      <table class="grid">
        <tr><th>Direction</th><th>Action</th><th>Proto</th><th>Source IP</th><th>Dest IP</th><th>Source Port</th><th>Dest Port</th><th></th></tr>
        ${rules.map(fwRuleRow).join("") || `<tr><td colspan="8" class="empty">No rules — traffic is accepted.</td></tr>`}
      </table>
      <div class="actions">
        <button class="btn" id="fw-add-rule">${ic.plus(14)} Add Rule</button>
        <button class="btn al-open" id="fw-logs">${ic.logs(14)} Access Logs</button>
      </div>

      <div class="section-title fw-cat" style="margin-top:18px"><span class="cat-ic">${ic.network()}</span>NAT Port Forwards</div>
      <table class="grid">
        <tr><th>WAN</th><th>Proto</th><th>WAN Port</th><th>Internal Guest</th><th>Internal Port</th><th></th></tr>
        ${forwards.map((f) => fwFwdRow(f, lanHosts, lanLabel)).join("") || `<tr><td colspan="6" class="empty">No port forwards.</td></tr>`}
      </table>
      <div class="actions"><button class="btn" id="fw-add-fwd">${ic.plus(14)} Add Forward</button></div>`);

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
      forwards.push({ wan_ip: "0.0.0.0", wan_port: 1, lan_ip: "", lan_port: 1, proto: "tcp" });
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
    `/proxy/${state.selectedConn}/networks/${encodeURIComponent(netName)}/firewall`)
    .then(async (fw) => {
      rules = fw.rules ?? [];
      forwards = fw.forwards ?? [];
      policy = fw.policy ?? blankPolicy();
      const h = await api.get<{ hosts: IpamHost[] }>(
        `/proxy/${conn}/networks/${encodeURIComponent(netName)}/hosts`).catch(() => null);
      lanHosts = h?.hosts ?? [];
      render();
    })
    .catch((e) => toast((e as Error).message, "bad"));

  $("#fw-cancel", el).onclick = () => el.remove();
  $("#fw-save", el).onclick = async () => {
    read();
    if (forwards.some((f) => !f.lan_ip)) return toast("Pick a guest for each port forward", "bad");
    try {
      await api.put(`/proxy/${state.selectedConn}/networks/${encodeURIComponent(netName)}/firewall`,
        { rules, forwards, policy });
      toast("Firewall applied");
      el.remove();
    } catch (e) {
      toast((e as Error).message, "bad");
    }
  };
}

async function loadVms() {
  if (!state.selectedConn) return;
  try {
    const vms = await api.get<DomainSummary[]>(`/proxy/${state.selectedConn}/domains`);
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
          <button class="btn icon" data-vm-edit="${esc(v.name)}" title="Edit Hardware">${ic.edit()}</button>
          <button class="btn danger icon" data-vm-del="${esc(v.name)}" title="Delete">${ic.trash()}</button>
        </div>
      </div>`).join("") || emptyState("VMs", "Create VM", "vm-create-empty"));
    const createBtn = $("#vm-create-empty");
    if (createBtn) createBtn.onclick = () => openCreateVm();
    // The whole card opens the guest; action buttons stop propagation.
    list.querySelectorAll("[data-vm-open]").forEach((card: Element) => {
      (card as HTMLElement).onclick = () => {
        state.selectedVm = (card as HTMLElement).dataset.vmOpen!;
        state.ctMode = false;
        state.vmTab = "summary";
        renderVm();
      };
    });
    list.querySelectorAll("[data-vm-toggle]").forEach((b: Element) => {
      (b as HTMLElement).onclick = async (e) => {
        e.stopPropagation();
        const n = (b as HTMLElement).dataset.vmToggle!;
        const start = (b as HTMLElement).dataset.vmAction === "start";
        await act(`/proxy/${state.selectedConn}/domains/${encodeURIComponent(n)}/${start ? "start" : "shutdown"}`, "POST");
        loadVms();
      };
    });
    // Jump straight to the console tab for that guest.
    list.querySelectorAll("[data-vm-console]").forEach((b: Element) => {
      (b as HTMLElement).onclick = (e) => {
        e.stopPropagation();
        leaveCurrentView();
        state.selectedVm = (b as HTMLElement).dataset.vmConsole!;
        state.ctMode = false;
        state.vmTab = "console";
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

// ---------------------------------------------------------------- VM detail

/// Enable the VM header's power actions for the guest's current state.
///
/// The header is painted once per view, and on a fresh load that happens
/// before the guest list has arrived, so it painted every guest as stopped and
/// nothing corrected it: the poll refreshes the list, not the header. This
/// runs whenever the list changes, which also covers a guest that changes
/// state on its own (shut down from inside, crashed).
function syncVmActions() {
  if (!state.selectedConn || !state.selectedVm) return;
  const guest = guestList(state.selectedConn)?.find((x) => x.name === state.selectedVm);
  const running = guest?.active === true;
  // The header is painted before the lists arrive, and a guest named by its
  // UUID (every new one) then shows the UUID; put its title in once known.
  const t = document.querySelector("#content .dtitle");
  if (t && guest?.title && t.textContent !== guest.title) t.textContent = guest.title;
  const set = (id: string, on: boolean) => {
    const b = document.getElementById(id) as HTMLButtonElement | null;
    if (b) b.disabled = !on;
  };
  set("vm-start", !running);
  for (const id of ["vm-shutdown", "vm-reboot", "vm-reset", "vm-destroy", "vm-vv"]) set(id, running);
}

async function renderVm() {
  if (!state.selectedConn || !state.selectedVm) return;
  if (isOffline(state.selectedConn)) return renderOffline();
  // VM view owns the pane: invalidates any in-flight resource render.
  beginRender();
  syncUrl();
  const content = $("#content");
  const vm = state.selectedVm;
  const ct = state.ctMode;
  const guest = guestList(state.selectedConn)?.find((x) => x.name === vm);
  const title = guest?.title || vm;
  const running = guest?.active === true;
  const privileged = ct && (guest as ContainerSummary | undefined)?.privileged === true;
  // A container has no display to hand to virt-viewer, and no reset. Its
  // Hardware tab is its own editor (CPU, memory, swap, root disk): the VM one
  // edits devices a container does not have.
  const restoreTabFocus = holdFocus(content, ".tab.active");
setHtml(content, `
    <div class="dhead"><span class="dhead-ic">${ct ? ic.container(20) : ic.vm(20)}</span><span class="dtitle">${esc(title)}</span>
      ${privileged ? `<span class="badge bad" title="No user namespace: its root is the host's root">privileged</span>` : ""}</div>
    <div class="tabbar">
      <button class="tab ${state.vmTab === "summary" ? "active" : ""}" data-vmtab="summary">${ic.datacenter(14)} Summary</button>
      <button class="tab ${state.vmTab === "console" ? "active" : ""}" data-vmtab="console">${ct ? ic.keyboard(14) : ic.vm(14)} Console</button>
      <button class="tab ${state.vmTab === "hardware" ? "active" : ""}" data-vmtab="hardware">${ic.cpu(14)} Hardware</button>
      <button class="tab ${state.vmTab === "options" ? "active" : ""}" data-vmtab="options">${ic.options(14)} Options</button>
      <button class="tab ${state.vmTab === "network" ? "active" : ""}" data-vmtab="network">${ic.network(14)} Network</button>
      <button class="tab ${state.vmTab === "xml" ? "active" : ""}" data-vmtab="xml">${ic.xml(14)} XML</button>
      <div class="tabbar-actions row-actions">
        <button class="btn icon" id="vm-start" title="Start" ${running ? "disabled" : ""}>${ic.start()}</button>
        <button class="btn icon" id="vm-shutdown" title="Shutdown" ${running ? "" : "disabled"}>${ic.stop()}</button>
        <button class="btn icon" id="vm-reboot" title="Reboot" ${running ? "" : "disabled"}>${ic.reboot()}</button>
        ${ct ? "" : `<button class="btn icon" id="vm-reset" title="Reset" ${running ? "" : "disabled"}>${ic.reset()}</button>`}
        <button class="btn icon" id="vm-destroy" title="Force Stop" ${running ? "" : "disabled"}>${ic.forceStop()}</button>
        ${ct ? "" : `<button class="btn icon" id="vm-vv" title="Open in virt-viewer" ${running ? "" : "disabled"}>${ic.download()}</button>`}
        ${canShareVm(state.selectedConn!, vm, ct) ? `<button class="btn icon" id="vm-share" title="Share">${ic.perms()}</button>` : ""}
        <button class="btn danger icon" id="vm-delete" title="Delete">${ic.trash()}</button>
      </div>
    </div>
    <div id="vm-body">${state.vmTab === "console" ? "" : skeleton("page")}</div>`);
  restoreTabFocus();
  content.querySelectorAll("[data-vmtab]").forEach((b: Element) => {
    (b as HTMLElement).onclick = () => {
      const next = (b as HTMLElement).dataset.vmtab as typeof state.vmTab;
      // Leaving the console must tear down its WebSocket and observer,
      // otherwise the session keeps streaming in the background.
      if (state.vmTab === "console" && next !== "console") closeConsole();
      state.vmTab = next;
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
  const base = guestBase(state.selectedConn, vm);
  $("#vm-start").onclick = () => act(`${base}/start`, "POST").then(refresh);
  $("#vm-shutdown").onclick = () => act(`${base}/shutdown`, "POST").then(refresh);
  const reset = $("#vm-reset");
  if (reset) reset.onclick = () => act(`${base}/reset`, "POST").then(refresh);
  $("#vm-destroy").onclick = () => act(`${base}/destroy`, "POST").then(refresh);
  $("#vm-reboot").onclick = () => act(`${base}/reboot`, "POST").then(refresh);
  $("#vm-delete").onclick = () => (ct ? deleteCt(vm) : deleteVm(vm));
  const share = $("#vm-share");
  if (share) share.onclick = () => void openShareVm(state.selectedConn!, vm, ct);
  const vvConn = state.selectedConn;
  const vv = $("#vm-vv");
  if (vv) vv.onclick = () => void downloadVv(vvConn, vm);
  // XML is read-only, so painting a cached answer first is safe. Summary shows
  // live statistics, which must never be painted stale, and the editing tabs
  // load once: a second paint would discard what the user typed meanwhile.
  if (state.vmTab === "xml") await fastLoad(() => renderVmBody(vm));
  else await renderVmBody(vm);
}

async function renderVmBody(vm: string) {
  if (!state.selectedConn) return;
  const conn = state.selectedConn; // narrowed copy for the nested closures below
  const body = $("#vm-body");
  // The pane can be gone: the poll may fire this just as a different view
  // replaces the content (or the user navigates mid-render). Bail rather than
  // dereference null — the previous `.parentElement?.` guarded the wrong hop.
  if (!body) return;
  // Only the Hardware tab stretches to fill the pane; every other tab scrolls
  // normally. Drop the class here and let the hardware branch re-add it.
  body.parentElement?.classList.remove("vm-fill");
  if (state.vmTab === "console" && state.ctMode) {
    // A container's console is a text terminal: its first tty, long-polled
    // through the daemon, with no graphics to zoom or keys to synthesise.
    setHtml(body, `
      <div class="console-stage term-stage" id="console-stage" tabindex="0">
        <div class="console-msg" id="console-msg">connecting…</div>
      </div>`);
    state.terminal ??= await import("./terminal");
    if (!stillCurrentVm(conn, vm, "console")) return;
    await state.terminal.attachTextConsole($("#console-stage"), guestBase(conn, vm, true), (msg: string | null) => {
      const m = $("#console-msg");
      if (m) {
        m.textContent = msg ?? "";
        m.classList.toggle("hidden", !msg);
      }
    });
    return;
  }
  if (state.vmTab === "options" && state.ctMode) {
    await renderCtOptions(body, conn, vm);
    return;
  }
  if (state.vmTab === "hardware" && state.ctMode) {
    await renderCtHardware(body, conn, vm);
    return;
  }
  if (state.vmTab === "console") {
    const zoomLabel = (z: number) => (z === 0 ? "Fit" : "100%");
    setHtml(body, `
      <div class="console-bar">
        <span class="console-group">
          <button class="btn" id="con-zoom-reset" title="Switch between Fit and 100%">${zoomLabel(state.consoleZoom)}</button>
        </span>
        <span class="console-group" style="margin-left:auto">
          <button class="btn icon" id="con-shorter" title="Shorter">${ic.shorter()}</button>
          <button class="btn icon" id="con-taller" title="Taller">${ic.taller()}</button>
          <button class="btn icon" id="con-full" title="Fullscreen (hold Esc to leave)">${ic.fit()}</button>
          <button class="btn icon" id="con-type" title="Type text into the guest">${ic.clipboard()}</button>
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
      const g = await api.get<{ graphics: { kind: string } | null }>(`${guestBase(state.selectedConn!, vm)}/graphics`);
      const kind = g.graphics?.kind ?? "vnc";
      const base = `${guestBase(state.selectedConn!, vm)}/console`;

      // SPICE is not RFB and has no usable browser client, so hand the guest
      // over to virt-viewer instead of pretending to render it here.
      if (kind !== "vnc") {
        for (const id of ["#con-zoom-reset", "#con-cad", "#con-type", "#con-full", "#con-taller", "#con-shorter"]) {
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
      });

      if (!con) {
        // The daemon refused to attach. Leave the toolbar visible but inert
        // rather than silently dead, and say why.
        for (const id of ["#con-zoom-reset", "#con-cad", "#con-type", "#con-full", "#con-taller", "#con-shorter"]) {
          const el = $(id);
          if (el) el.disabled = true;
        }
        setHint("Console unavailable — controls disabled until it connects.");
        return;
      }

      setHint(isCoarsePointer()
        ? "Tap to click, long-press to right-click, two fingers to scroll."
        : "Click the screen to grab input.");

      const toggleFit = () => {
        const next = con.getZoom() === 0 ? 1 : 0;
        con.setZoom(next);
        state.consoleZoom = next;
        const b = $("#con-zoom-reset");
        if (b) b.textContent = zoomLabel(next);
      };
      $("#con-zoom-reset").onclick = toggleFit;

      // The stage itself goes fullscreen, so the guest gets the whole monitor.
      // Keyboard Lock (Chromium) hands Esc to the guest; holding it leaves.
      const stageEl = $("#console-stage");
      $("#con-full").onclick = async () => {
        try {
          if (document.fullscreenElement) await document.exitFullscreen();
          else {
            await stageEl.requestFullscreen();
            stageEl.focus();
            void (navigator as any).keyboard?.lock?.(["Escape"]);
          }
        } catch (e) {
          toast((e as Error).message, "bad");
        }
      };
      stageEl.onfullscreenchange = () => {
        if (!document.fullscreenElement) (navigator as any).keyboard?.unlock?.();
      };

      $("#con-cad").onclick = () => void con.sendCtrlAltDel();
      $("#con-type").onclick = () => void typeIntoGuest(con);

      // Re-fit when the pane resizes, so the guest stays centred.
      const stage = $("#console-stage");
      try {
        const saved = localStorage.getItem("console-height");
        if (saved) stage.style.height = saved;
      } catch { /* storage blocked: keep the default height */ }
      const resizeBy = (dy: number) => {
        const h = Math.min(4000, Math.max(160, stage.offsetHeight + dy));
        stage.style.height = `${h}px`;
        try { localStorage.setItem("console-height", `${h}px`); } catch { /* not saved */ }
      };
      $("#con-taller").onclick = () => resizeBy(80);
      $("#con-shorter").onclick = () => resizeBy(-80);
      const ro = new ResizeObserver(() => con.refit());
      ro.observe(stage);
      state.consoleResizeObserver?.disconnect();
      state.consoleResizeObserver = ro;

    } catch (e) {
      const m = $("#console-msg");
      if (m) m.textContent = (e as Error).message;
    }
    return;
  }
  if (state.vmTab === "options") {
    try {
      const [{ xml }, info] = await Promise.all([
        api.get<{ xml: string }>(`${guestBase(state.selectedConn!, vm)}/xml`),
        api.get<any>(`${guestBase(state.selectedConn!, vm)}`),
      ]);
      const pick = (re: RegExp, fallback = "—") => xml.match(re)?.[1] ?? fallback;
      const title = unescapeXml(pick(/<title>([^<]*)<\/title>/, ""));
      const description = unescapeXml(pick(/<description>([^<]*)<\/description>/, ""));
      // Boot order: disk then CD, as a fresh guest boots, plus whatever else
      // has been added. libvirt tries the <boot> elements top to bottom.
      const bootOrder: string[] = [...xml.matchAll(/<boot dev=["']([^"']*)["']\s*\/>/g)].map((m) => m[1]);
      if (bootOrder.length === 0) bootOrder.push("hd", "cdrom");
      const bootNames = bootTargets(xml);
      const bootDisk = xml.match(/<disk\b[^>]*device=['"]disk['"][\s\S]*?<\/disk>/)?.[0] ?? "";
      const bootSrc = bootDisk.match(/<source\b[^>]*>/)?.[0] ?? "";
      const bootVolume = bootSrc.match(/volume=['"]([^'"]+)['"]/)?.[1] ?? bootSrc.match(/file=['"]([^'"]+)['"]/)?.[1]?.split("/").pop();
      const bootPool = bootSrc.match(/pool=['"]([^'"]+)['"]/)?.[1];
      if (bootVolume && bootPool) {
        const vols = await api.get<VolumeSummary[]>(`/proxy/${state.selectedConn}/pools/${encodeURIComponent(bootPool)}/volumes`).catch(() => []);
        const nickname = vols.find((v) => v.name === bootVolume)?.title;
        if (nickname) bootNames.hd = `${bootNames.hd} — ${nickname}`;
      }
      // A network boot entry is named by its network, which `bootTargets`
      // reports as the libvirt name -- or as the bridge, for a guest attached
      // by one. Swap in the nickname the rest of the page uses, keeping the
      // MAC: the entry reads "LAN 52:54:...". Uses the bare `networkName`, not
      // the formatted label, so the substitution does not depend on a
      // delimiter.
      if (bootNames.networkName && bootNames.network) {
        const bootNet = (treeNets[state.selectedConn!] ?? []).find((n) =>
          n.name === bootNames.networkName || n.bridge === bootNames.networkName);
        bootNames.network = bootNames.network.replace(
          bootNames.networkName,
          displayName(treeNets[state.selectedConn!], bootNet?.name ?? bootNames.networkName),
        );
      }
      const machine = pick(/machine='([^']*)'/);
      const arch = pick(/arch='([^']*)'/);
      const onPoweroff = pick(/<on_poweroff>([^<]*)<\/on_poweroff>/);
      const onReboot = pick(/<on_reboot>([^<]*)<\/on_reboot>/);
      const onCrash = pick(/<on_crash>([^<]*)<\/on_crash>/);
      const running = info.active === true;

      setHtml(body, `
        <div class="actions">
          <button class="btn primary" id="opt-save">${ic.edit(14)} Save Options</button>
          ${running ? `<span class="hw-locked">Changes apply after the guest is restarted.</span>` : ""}
        </div>
        <div class="hwtable">
          <div class="hwrow">
            <span class="hwrow-ic">${ic.edit()}</span>
            <div class="hwrow-label"><b>Nickname</b></div>
            <div class="hwctl"><input id="opt-title" value="${esc(title)}" placeholder="short label"></div>
          </div>
          <div class="hwrow">
            <span class="hwrow-ic">${ic.options()}</span>
            <div class="hwrow-label"><b>Description</b></div>
            <div class="hwctl"><textarea id="opt-desc" rows="2" placeholder="free-form notes">${esc(description)}</textarea></div>
          </div>
          <div class="hwrow">
            <span class="hwrow-ic">${ic.disk()}</span>
            <div class="hwrow-label"><b>Boot Order</b><i>Tried top to bottom</i></div>
            <div class="hwctl">
              <div id="opt-boot-list" class="boot-list"></div>
              <div class="actions" style="margin:8px 0 0">
                <select id="opt-boot-add"></select>
                <button class="btn" id="opt-boot-add-btn">${ic.plus(14)} Add Device</button>
              </div>
            </div>
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

      // Reorder with the arrows, remove with the bin, add from the menu; the
      // sequence is written into <os> on save.
      const bootList = $("#opt-boot-list");
      const bootAdd = $("#opt-boot-add") as HTMLSelectElement | null;
      const bootAddBtn = $("#opt-boot-add-btn") as HTMLButtonElement | null;
      const paintBoot = () => {
        if (!bootList) return;
        // The dropdown shrinks as devices are added and grows again on remove,
        // so it is repainted with the list rather than baked into the page.
        // An empty list disables Add: every kind is already in the order.
        const choices = bootChoices(bootOrder, bootNames);
        if (bootAdd) {
          setHtml(bootAdd, choices.map((c) => `<option value="${esc(c.value)}">${esc(c.label)}</option>`).join(""));
        }
        if (bootAddBtn) {
          bootAddBtn.disabled = choices.length === 0;
          bootAddBtn.title = choices.length ? "" : "Every boot device is already listed";
        }
        setHtml(bootList, bootOrder.map((d, i) => `
          <div class="boot-row" data-boot-i="${i}">
            <span class="boot-order">${i + 1}</span>
            <span class="boot-name"><b>${esc(bootNames[d] ?? BOOT_LABEL[d] ?? d)}</b><i>${esc(BOOT_LABEL[d] ?? d)}</i></span>
            <span class="boot-acts">
              <button class="btn icon" data-boot-up ${i === 0 ? "disabled" : ""} title="Move Up">${ic.up(14)}</button>
              <button class="btn icon" data-boot-down ${i === bootOrder.length - 1 ? "disabled" : ""} title="Move Down">${ic.down(14)}</button>
              <button class="btn danger icon" data-boot-del title="Remove">${ic.trash()}</button>
            </span>
          </div>`).join("") || `<p class="hint">Nothing to boot from; libvirt uses its own order.</p>`);
        const idx = (b: Element) => Number((b.closest("[data-boot-i]") as HTMLElement).dataset.bootI);
        bootList.querySelectorAll("[data-boot-up]").forEach((b: Element) => {
          (b as HTMLElement).onclick = () => {
            const i = idx(b);
            if (i > 0) { [bootOrder[i - 1], bootOrder[i]] = [bootOrder[i], bootOrder[i - 1]]; paintBoot(); }
          };
        });
        bootList.querySelectorAll("[data-boot-down]").forEach((b: Element) => {
          (b as HTMLElement).onclick = () => {
            const i = idx(b);
            if (i < bootOrder.length - 1) { [bootOrder[i], bootOrder[i + 1]] = [bootOrder[i + 1], bootOrder[i]]; paintBoot(); }
          };
        });
        bootList.querySelectorAll("[data-boot-del]").forEach((b: Element) => {
          (b as HTMLElement).onclick = () => { bootOrder.splice(idx(b), 1); paintBoot(); };
        });
      };
      paintBoot();
      $("#opt-boot-add-btn")?.addEventListener("click", () => {
        const sel = $("#opt-boot-add") as HTMLSelectElement | null;
        // Guarded: with every kind already listed the select is empty, and
        // pushing its empty value would add a `<boot dev=''/>`.
        if (sel?.value) { bootOrder.push(sel.value); paintBoot(); }
      });

      $("#opt-save")?.addEventListener("click", async () => {
        const newTitle = $("#opt-title").value.trim();
        const newDesc = $("#opt-desc").value.trim();
        const target = vm;

        // Rewrite only the elements this form owns, leaving the rest of the
        // definition untouched.
        let next = xml
          .replace(/\s*<title>[^<]*<\/title>/, "")
          .replace(/\s*<description>[^<]*<\/description>/, "")
          .replace(/<name>[^<]*<\/name>/, `<name>${esc(target)}</name>`);
        // The whole boot sequence is rewritten: its document order is the
        // order libvirt tries the devices, and a guest that had none gets one
        // (libvirt's implicit default is disk-then-CD, which is what this
        // starts from).
        const boots = bootOrder.map((d) => `<boot dev='${esc(d)}'/>`).join("");
        next = next.replace(/\n?[ \t]*<boot dev=["'][^"']*["']\s*\/>/g, "");
        if (boots) next = next.replace(/(<\/os>)/, `  ${boots}\n  $1`);
        const meta = [
          newTitle ? `  <title>${esc(newTitle)}</title>` : "",
          newDesc ? `  <description>${esc(newDesc)}</description>` : "",
        ].filter(Boolean).join("\n");
        if (meta) {
          // libvirt requires title/description right after <name>.
          next = next.replace(/(<name>[^<]*<\/name>)/, `$1\n${meta}`);
        }
        void runTask(`Update options for ${displayName(guestList(state.selectedConn!), target)}`, async () => {
          await api.putXml(`${guestBase(state.selectedConn!, target)}/xml`, next);
        }, { done: `Options for ${displayName(guestList(state.selectedConn!), target)} saved`, notify: toast });
      });
    } catch (e) {
      setHtml(body, `<div class="empty">${esc((e as Error).message)}</div>`);
    }
    return;
  }

  if (state.vmTab === "network") {
    // Everything about this guest's networking on one page: its interfaces
    // (hot-added and removed, live), their DHCP reservations and rate limits,
    // and its firewall. Interfaces are read from the live guest each render,
    // so a NIC added from the Hardware tab or Create VM simply appears here.
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
        api.get<{ interfaces: Nic[] }>(`${guestBase(conn, vm)}/interfaces`),
        api.get<NetworkSummary[]>(`/proxy/${conn}/networks`),
        api.get<{ active: boolean }>(`${guestBase(conn, vm)}`),
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
    // A container's interface is a veth: it has no model, and it is added and
    // removed at its next start (the response says when), so only a VM gets
    // the live/shut-off line.
    const ct = state.ctMode;
    const liveNote = ct ? "" : running
      ? "Changes apply to the running guest immediately and are kept for its next boot."
      : "The guest is shut off: changes are saved to its definition.";

    setHtml(body, `
      <div class="section-title fw-cat"><span class="cat-ic">${ic.network()}</span>Interfaces</div>
      ${liveNote ? `<div class="dsub" style="margin-bottom:8px">${liveNote}</div>` : ""}
      <table class="grid" id="vmnet-nics">
        <tr><th>Device</th><th>MAC</th><th>Network</th>${ct ? "" : "<th>Model</th>"}<th>IPAM Address</th><th>RX Limit</th><th>TX Limit</th><th class="col-actions"></th></tr>
        ${nics.map((n, i) => {
          const r = reservation(n);
          const hasDhcp = !!(n.network && netIpam.has(n.network));
          return `<tr data-nic="${i}">
            <td>${esc(n.dev ?? "—")}</td>
            <td><code>${esc(n.mac ?? "—")}</code></td>
            <td>${esc(n.network ? displayName(nets, n.network) : "—")}</td>
            ${ct ? "" : `<td>${esc(n.model ?? "—")}</td>`}
            <td>${r
              ? `${esc(r.ip)}${r.dynamic ? ` <span class="state" title="Assigned from the range when the guest first asked; kept until the VM is deleted">auto</span>` : ""}`
              : `<span class="state">${hasDhcp ? "Not yet assigned" : "No DHCP"}</span>`}</td>
            <td>${fmtRateCell(n.rx_mbps)}</td>
            <td>${fmtRateCell(n.tx_mbps)}</td>
            <td class="col-actions">
              <button class="btn icon" data-nic-ipam="${i}" title="IPAM Address" ${hasDhcp ? "" : "disabled"}>${ic.edit()}</button>
              <button class="btn icon" data-nic-rate="${i}" title="Rate Limit">${ic.autostart()}</button>
              <button class="btn danger icon" data-nic-del="${i}" title="Remove Interface">${ic.trash()}</button>
            </td></tr>`;
        }).join("") || `<tr><td colspan="${ct ? 7 : 8}" class="empty">No network interfaces.</td></tr>`}
      </table>
      <div class="actions"><button class="btn" id="vmnet-add">${ic.plus(14)} Add Interface</button></div>
      <div class="section-title fw-cat" style="margin-top:22px"><span class="cat-ic">${ic.firewall()}</span>Firewall</div>
      <div id="vmnet-fw">${skeleton("table")}</div>`);

    const reload = () => { if (stillCurrentVm(conn, vm, "network")) void renderVmBody(vm); };

    $("#vmnet-add", body).onclick = () => {
      const el = openModal("Add Interface", `
        <label class="field"><span>Network</span><select id="nic-net">
          <option value="">None</option>
          ${nets.map((n) => `<option value="${esc(n.name)}">${esc(n.title || n.name)}${n.active ? "" : " (inactive)"}</option>`).join("")}
        </select></label>
        ${ct ? "" : `<label class="field"><span>Model</span><select id="nic-model">
          <option value="virtio">virtio</option><option value="e1000e">e1000e</option><option value="rtl8139">rtl8139</option>
        </select></label>`}
        <label class="field"><span>MAC Address (optional)</span><input id="nic-mac" placeholder="generated if blank"></label>
        ${ct ? "" : `<div class="dsub">${running ? "Hot-added to the running guest." : "Added to the guest's definition."}</div>`}`,
        `<button class="btn primary" id="nic-ok">${ic.plus(14)} Add</button>`);
      $("#nic-ok", el).onclick = async () => {
        try {
          const r = await api.post<{ mac: string; warning?: string; pending?: boolean }>(`${guestBase(conn, vm)}/interfaces`, {
            network: ($("#nic-net", el) as HTMLSelectElement).value,
            // A container's interface is a veth: it has no model.
            model: ct ? undefined : ($("#nic-model", el) as HTMLSelectElement | null)?.value,
            mac: ($("#nic-mac", el) as HTMLInputElement).value.trim() || undefined,
          });
          el.remove();
          // Never claim an interface the running container does not have yet.
          toast(r.warning ?? (r.pending
            ? `Interface ${r.mac} added: the running container gets it at its next start`
            : `Interface ${r.mac} added`), r.warning ? "bad" : "ok");
          reload();
        } catch (e) { toast((e as Error).message, "bad"); }
      };
    };

    body.querySelectorAll("[data-nic-del]").forEach((b: Element) => {
      (b as HTMLElement).onclick = async () => {
        const n = nics[Number((b as HTMLElement).dataset.nicDel)];
        if (!n?.mac) return;
        const ok = await askConfirm("Remove Interface",
          `Remove ${n.mac} (${n.network ?? "no network"})${running ? " from the running guest" : ""}?`, "Remove", true);
        if (!ok) return;
        try {
          const r = await api.del<{ pending: boolean; warning?: string }>(
            `${guestBase(conn, vm)}/interfaces/${encodeURIComponent(n.mac)}`);
          // An unplug is a request to the guest OS; say so when it is still
          // holding the device, instead of claiming it is gone.
          toast(r.pending
            ? (ct ? "Removed from the definition: the running container keeps it until its next start"
              : "Removed from the definition; the running guest has not released it yet")
            : r.warning ?? "Interface removed", r.pending || r.warning ? "bad" : "ok");
          reload();
        } catch (e) { toast((e as Error).message, "bad"); }
      };
    });

    body.querySelectorAll("[data-nic-rate]").forEach((b: Element) => {
      (b as HTMLElement).onclick = () => {
        const n = nics[Number((b as HTMLElement).dataset.nicRate)];
        if (!n?.mac) return;
        const el = openModal(`Rate Limit — ${n.mac}`, `
          <label class="field"><span>RX limit, Mbps (to the guest; blank = unlimited)</span>
            <input id="rate-rx" type="number" min="0" step="any" value="${n.rx_mbps ?? ""}"></label>
          <label class="field"><span>TX limit, Mbps (from the guest; blank = unlimited)</span>
            <input id="rate-tx" type="number" min="0" step="any" value="${n.tx_mbps ?? ""}"></label>
          ${ct ? "" : `<div class="dsub">${running ? "Applied to the running guest immediately." : "Applied when the guest next starts."}</div>`}`,
          `<button class="btn primary" id="rate-ok">Apply</button>`);
        const num = (id: string) => {
          const v = ($(id, el) as HTMLInputElement).value.trim();
          return v === "" ? undefined : Number(v);
        };
        $("#rate-ok", el).onclick = async () => {
          try {
            const res = await api.put<{ pending?: boolean }>(`${guestBase(conn, vm)}/interfaces/${encodeURIComponent(n.mac!)}/bandwidth`,
              { rx_mbps: num("#rate-rx"), tx_mbps: num("#rate-tx") });
            el.remove();
            // Never claim a limit the running guest is not enforcing yet.
            toast(res?.pending ? "Rate limit saved: it takes effect when the guest next starts" : "Rate limit applied");
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
        const el = openModal(`IPAM Address — ${n.mac}`, `
          <label class="field"><span>IP address on ${esc(n.network ? displayName(nets, n.network) : "—")} (${esc(ipam.gateway)}/${ipam.prefix})</span>
            <input id="ipam-ip" value="${esc(suggested)}" placeholder="blank = assign automatically"></label>
          <label class="field"><span>Hostname (optional)</span>
            <input id="ipam-name" value="${esc(current?.name ?? vm)}"></label>
`,
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
        `${guestBase(state.selectedConn!, vm)}/firewall`);
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
        <div class="section-title fw-cat"><span class="cat-ic">${ic.firewall()}</span>Default Policies</div>
        ${fwPolicyRow(vmPolicy)}
        <div class="section-title fw-cat"><span class="cat-ic">${ic.options()}</span>Rules</div>
        <table class="grid">
          <tr><th>Direction</th><th>Action</th><th>Proto</th><th>Source IP</th><th>Dest IP</th><th>Source Port</th><th>Dest Port</th><th></th></tr>
          ${vmRules.map(fwRuleRow).join("") || `<tr><td colspan="8" class="empty">No rules — traffic is accepted.</td></tr>`}
        </table>
        <div class="actions">
          <button class="btn" id="vmfw-add">${ic.plus(14)} Add Rule</button>
          <button class="btn primary" id="vmfw-apply">${ic.firewall(14)} Apply</button>
          <button class="btn al-open" id="vmfw-logs">${ic.logs(14)} Access Logs</button>
        </div>`);
      $("#vmfw-logs", rulesBody).onclick = () => openAccessLogs(conn, vm, ct);
      attachRuleIpSuggest(rulesBody, () => ipCandidates);
      $("#vmfw-add", rulesBody).onclick = () => {
        readRules();
        vmRules.push(blankRule());
        renderRules();
      };
      $("#vmfw-apply", rulesBody).onclick = async () => {
        readRules();
        try {
          await api.put(`${guestBase(state.selectedConn!, vm)}/firewall`, { rules: vmRules, policy: vmPolicy });
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

  if (state.vmTab === "hardware") {
    // The hardware editor is embedded here — no read-only table and no
    // "Edit Hardware" click. Devices are edited in place and saved.
    await openHardwareEditor({ vm, inline: true });
    return;
  }
  if (state.vmTab === "xml") {
    try {
      const { xml } = await api.get<{ xml: string }>(`${guestBase(state.selectedConn!, vm)}/xml`);
setHtml(body, `<div class="cmd">${esc(xml)}</div>`);
    } catch (e) {
setHtml(body, `<div class="empty">${esc((e as Error).message)}</div>`);
    }
    return;
  }
  // summary: six cards — CPU, Memory, Disks, Network, Firewall, Hardware.
  const ct = state.ctMode;
  try {
    const [d, s, xr, fw, nic] = await Promise.all([
      api.get<any>(`${guestBase(state.selectedConn!, vm)}`),
      api.get<DomainStats>(`${guestBase(state.selectedConn!, vm)}/stats`),
      // A container's summary reads its rootfs and NICs from the daemon's own
      // records, not from a definition, so it needs no XML.
      ct ? Promise.resolve({ xml: "" }) : api.get<{ xml: string }>(`${guestBase(state.selectedConn!, vm)}/xml`),
      api.get<{ rules: FirewallRule[]; policy: FirewallPolicy }>(
        `${guestBase(state.selectedConn!, vm)}/firewall`)
        .catch(() => ({ rules: [] as FirewallRule[], policy: blankPolicy() })),
      api.get<{ interfaces: { dev: string | null; mac: string | null; model: string | null; network?: string | null; rx_bytes: number; tx_bytes: number }[] }>(
        `${guestBase(state.selectedConn!, vm)}/interfaces`)
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
    // A container's root filesystem is not a <disk>: it is one image file in a
    // pool, which the daemon reports by path.
    const rootfs: string = ct ? (d?.rootfs ?? "") : "";
    if (rootfs) {
      disks.push({ dev: "rootfs", kind: "disk", pool: "", volume: "", file: rootfs, label: rootfs.split("/").pop() ?? rootfs });
    }
    // A NIC names the network's bridge now; `nicNetwork` maps it back to the
    // network key the rest of the page (and its titles) is keyed by.
    const nics = [...xml.matchAll(/<interface\b[^>]*type=['"](?:network|bridge)['"][\s\S]*?<\/interface>/g)].map((m) => {
      const block = m[0];
      return {
        net: nicNetwork(treeNets[conn], block.match(/<source\b[^>]*>/)?.[0] ?? ""),
        model: xattr(block.match(/<model\b[^>]*>/)?.[0] ?? "", "type"),
      };
    });
    const devices = devicesFromXml(xml);
    // The summary shows a NIC by its network, but its definition names the
    // bridge. The bridge stays on the device, so the editor still round-trips
    // one this app does not know.
    for (const d of devices) {
      if (d.kind === "interface" && !d.fields.network && d.fields.bridge) {
        d.fields.network = bridgeNetwork(treeNets[conn] ?? [], d.fields.bridge);
      }
    }
    const bios = /pflash|OVMF/i.test(xml) ? "UEFI (OVMF)" : "BIOS (SeaBIOS)";
    const display = /<graphics[^>]*type=['"]spice/.test(xml) ? "spice" : "vnc";

    // Allocation/usage for volume-backed disks comes from the pool's volume
    // table; file-backed disks have no volume record, so they show path only.
    const volUsage = new Map<string, { capacity: number; allocation: number }>();
    const backingPools = new Set(disks.map((x) => x.pool).filter(Boolean) as string[]);
    // The rootfs names a path, not a pool: look in every pool for it, by path
    // or by the name the daemon gives it (`ct-<uuid>.raw`).
    if (rootfs) for (const p of treePools[conn] ?? []) backingPools.add(p.name);
    const rootfsVol = ct ? `ct-${d?.uuid ?? vm}.raw` : "";
    for (const pool of backingPools) {
      try {
        const vols = await api.get<VolumeSummary[]>(
          `/proxy/${state.selectedConn}/pools/${encodeURIComponent(pool)}/volumes`);
        for (const v of vols) {
          const u = { capacity: v.capacity ?? 0, allocation: v.allocation ?? 0 };
          volUsage.set(`${pool}/${v.name}`, u);
          if (rootfs && (v.path === rootfs || v.name === rootfsVol)) volUsage.set(`file:${rootfs}`, u);
        }
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
      : selDisk?.file ? volUsage.get(`file:${selDisk.file}`) : undefined;
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
      model: x.model || nics[i]?.model || (ct ? "veth" : "virtio"),
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
      // `ct` checks the container's permission; the series are the same.
      const q: Record<string, string> = ct ? { conn, name: vm, ct: "true" } : { conn, name: vm };
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
    const hwStrip = ct ? [
      { icon: ic.container(), label: "Container", sum: `${s.vcpus} vCPU` },
      { icon: ic.lock(), label: "Isolation", sum: d?.privileged ? "privileged (host root)" : "user namespace" },
      { icon: ic.disk(), label: "Root Filesystem", sum: rootfs.split("/").pop() || "—" },
      ...(nic.interfaces ?? []).map((x) => ({
        icon: ic.network(), label: "Network", sum: x.network ? displayName(treeNets[conn], x.network) : (x.mac ?? "—"),
      })),
    ] : [
      { icon: ic.cpu(), label: "BIOS", sum: bios },
      { icon: ic.vm(), label: "Graphics", sum: display },
      ...devices.map((x) => ({
        icon: ic[hwType(x.kind)?.icon ?? "cpu"](),
        label: hwType(x.kind)?.label ?? x.kind,
        sum: deviceSummary(x),
      })),
    ];

    setHtml(body, `
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
          <div class="chart-axis">${rangeSelect()}<span>now</span></div>
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
          <div class="chart-axis">${rangeSelect()}<span>now</span></div>
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
          <div class="chart-axis">${rangeSelect()}<span>now</span></div>
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
          <div class="chart-axis">${rangeSelect()}<span>now</span></div>
        </div>
        <div class="metric-foot">
          <span>${selNic?.dev ?? "—"}</span>
          <span>${esc(selNic?.model ?? (ct ? "veth" : "virtio"))}</span>
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

    <div class="section-title fw-cat"><span class="cat-ic">${ic.cpu()}</span>Hardware</div>
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

// ---------------------------------------------------------------- dialogs

/// Type free text into a running guest as keystrokes.
///
/// A textarea rather than a keystroke-sniffing listener, because the point is
/// to get text IN from outside the guest (a password manager, a config
/// snippet, a long URL). The console still owns real typing; this is the paste
/// half, which RFB clipboard would only deliver to a guest running a VNC
/// clipboard agent, so it would look broken on all the others.
///
/// Multiline is kept: newlines become Return in the daemon, so a pasted script
/// runs line by line like it was typed.
async function typeIntoGuest(con: { typeText(t: string): Promise<void> }) {
  const text = await askMultiline(
    "Type into guest",
    "Text",
    "Typed as keystrokes — the guest sees a real keyboard.",
  );
  if (text === null || text === "") return;
  await runTask(
    `Type ${text.length} character(s) into the guest`,
    () => con.typeText(text),
    { done: "Text typed into the guest", notify: toast },
  );
}

// ---------------------------------------------------------------- tasks
// ---------------------------------------------------------------- polling

// Whether a touch gesture or its inertia is still moving something. A poll
// rebuilds whole tables with setHtml, and replacing the node the browser is
// scrolling cancels the gesture: on a phone the list stopped dead under the
// finger, every time a refresh landed mid-flick. Cheap enough to ask every
// tick, and a skipped tick is just one second's worth of staleness.
let fingerDown = false;
let lastScrollAt = 0;
document.addEventListener("pointerdown", () => { fingerDown = true; }, true);
document.addEventListener("pointerup", () => { fingerDown = false; }, true);
document.addEventListener("pointercancel", () => { fingerDown = false; }, true);
// A lost pointer (tab hidden, window blurred mid-drag) never reports up.
window.addEventListener("blur", () => { fingerDown = false; });
document.addEventListener("scroll", () => { lastScrollAt = Date.now(); }, true);
function busyScrolling(): boolean {
  return fingerDown || Date.now() - lastScrollAt < 250;
}

function startPolling() {
  if (state.pollTimer) window.clearInterval(state.pollTimer);
  // One round at a time: a tick that finds reads still undelivered does
  // nothing. `api.ts` delivers a round at most once a second, and the tick is
  // finer than that so the next round starts as soon as the last one has
  // landed, which keeps the cadence at the delivery pace instead of at every
  // other tick.
  state.pollTimer = window.setInterval(() => {
    if (!state.user || pendingReads() > 0) return;
    // A repaint replaces the node the browser is scrolling, which detaches the
    // touch gesture: a drag over a table would stop dead mid-flick.
    if (busyScrolling()) return;
    pollWindow(pollRound);
  }, 250);
}

/// One poll round: the reads it starts are delivered together at the next
/// update slot (`api.ts`), so the views repaint once.
function pollRound() {
  void refreshHealth();
  // A down server's views show the offline panel; polling its daemon would
  // only fail again and paint the raw error over it.
  if (isOffline(state.selectedConn)) return;
  // Keep the sidebar's guest states live regardless of the open pane.
  void refreshTreeVms();
  if (!state.selectedConn && !state.selectedVm && state.dcTab === "dashboard") {
    loadDcDashboard();
  } else if (state.selectedConn && !state.selectedVm) {
    // Resource pages poll their own data; only repaint when they are the
    // view actually on screen.
    if (state.selectedNet) renderNetPage();
    else if (state.selectedPool) renderPoolPage();
    else if (state.connTab === "dashboard") loadDashboard();
    else if (state.connTab === "vms") loadVms();
    else if (state.connTab === "containers") loadCts();
    else if (state.connTab === "networks") loadNetworks();
    else if (state.connTab === "storage") loadPools();
    // The shell is a live session, not data to re-read; nothing to poll.
    else if (state.connTab === "firewall") loadFilters();
  } else if (state.selectedConn && state.selectedVm && state.vmTab === "summary") {
    renderVmBody(state.selectedVm);
  }
}

// ---------------------------------------------------------------- helpers

initIndicators();
boot();
