import { api } from "../api";
import type { DomainSummary, NetworkSummary, PoolSummary } from "../api";
import { ic } from "../icons";
import { skeleton } from "../skeleton";
import { holdFocus } from "../focus";
import { poolUsed } from "../pool-usage";
import { addHostSample, fmtBytes, pct, sparkline } from "../stats";
import type { HostStats, Timing } from "../stats";
import { isLive, loadHistory, range, rangeSelect } from "../history";
import { esc } from "../xml";
import { state, treeVms } from "../app/state";
import { emptyState } from "../app/util";
import { act, deleteVm } from "../app/actions";
import { hooks } from "../app/hooks";
import { beginRender, clearResourceSelection, fastLoad, leaveCurrentView, syncUrl } from "../app/nav";
import { $, setHtml } from "../app/dom";
import { openHardwareEditor } from "../forms/hardware-editor";
import { openCreateVm } from "../forms/create-vm";
import { openCreatePool, openCreateResource } from "../forms/create-resource";
import { openCreateNetwork } from "../forms/network-form";
import { canAdmin, isOffline, renderOffline } from "../app/boot";
import { renderTree } from "../app/tree";
import { loadNetworks } from "./network-page";
import { loadPools } from "./pool-page";
import { loadCts, openCreateCt } from "./containers";
import { bindDashOpeners } from "./resource-common";
import { openAccessLogs } from "./access-logs";
import { openFirewallEditor } from "./firewall-editor";

// ---------------------------------------------------------------- connection view

export async function renderConn() {
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

export async function renderConnBody() {
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
    state.terminal ??= await import("../terminal");
    await state.terminal.attachDaemonShell($("#shell-stage"), `/proxy/${conn}/shell`, msg);
  })();
}

/// Daemon dashboard: host CPU/RAM with sparklines, then counts and summaries
/// for VMs, networks, and storage pools. Reuses the existing list endpoints.
export async function loadDashboard() {
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
export async function loadFilters() {
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

export async function loadVms() {
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
        hooks.renderVm();
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
        void hooks.renderVm();
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
