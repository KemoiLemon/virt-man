import { api } from "./api";
import type { Connection, NetworkSummary, PoolSummary } from "./api";
import { ic } from "./icons";
import { skeleton } from "./skeleton";
import { holdFocus } from "./focus";
import { poolUsed } from "./pool-usage";
import { addDiskSample, addHostSample, fmtBytes, pct, sparkline } from "./stats";
import type { HostStats, Timing } from "./stats";
import { isLive, loadHistory, range, rangeSelect } from "./history";
import { esc } from "./xml";
import { state, treeCts, treeNets, treePools, treeVms } from "./state";
import { emptyState } from "./util";
import { act, deleteNetwork, deletePool } from "./actions";
import { hooks } from "./hooks";
import { beginRender, leaveCurrentView, syncUrl } from "./nav";
import { openUpload } from "./transfers";
import { $, setHtml } from "./dom";
import { openHardwareEditor } from "./hardware-editor";
import { openCreateVm } from "./create-vm";
import { dcCreate, openCreatePool } from "./create-resource";
import { openCreateNetwork, openEditNetwork } from "./network-form";
import { refreshTreeVms, renderTree, selectNetRow, selectPoolRow } from "./tree";
import { bindCtCards, ctCard, openCreateCt } from "./containers";
import { netAction, netCard } from "./network-page";
import { poolCard } from "./pool-page";
import { bindDashOpeners } from "./resource-common";

// ---------------------------------------------------------------- datacenter

export function renderDc() {
  beginRender();
  syncUrl();
  const content = $("#content");
  // Real tabbar; the Users tab is hidden when the local app disables auth.
  // Actions live on the tab bar itself, right-aligned after the tabs.
  const tabs = `<div class="tabbar">
      <button class="tab ${state.dcTab === "dashboard" ? "active" : ""}" data-dctab="dashboard">${ic.datacenter(14)} Dashboard</button>
      <button class="tab ${state.dcTab === "guests" ? "active" : ""}" data-dctab="guests">${ic.vms(14)} Guests</button>
      <button class="tab ${state.dcTab === "containers" ? "active" : ""}" data-dctab="containers">${ic.container(14)} Containers</button>
      <button class="tab ${state.dcTab === "networks" ? "active" : ""}" data-dctab="networks">${ic.network(14)} Networks</button>
      <button class="tab ${state.dcTab === "storage" ? "active" : ""}" data-dctab="storage">${ic.storage(14)} Storage</button>
    </div>`;
  // Aggregate cards across every known server, from the tree's guest cache.
  const allVms = Object.values(treeVms).flat();
  const runningVms = allVms.filter((v) => v.active).length;
  // Summary cards head the Servers and Guests tabs. Four tiles either way, so
  // the two are the same widget with their nouns substituted.
  const quickStats = (total: number, running: number, noun: string, icon: () => string) => `<div class="cards">
        <div class="card">
          <span class="card-ic">${ic.server(20)}</span>
          <div><b>${state.connections.length}</b><i>SERVERS</i></div>
        </div>
        <div class="card">
          <span class="card-ic">${icon()}</span>
          <div><b>${total}</b><i>${noun}</i></div>
        </div>
        <div class="card ${running > 0 ? "ok" : ""}">
          <span class="card-ic">${ic.start(20)}</span>
          <div><b>${running}</b><i>RUNNING</i></div>
        </div>
        <div class="card">
          <span class="card-ic">${ic.stop(20)}</span>
          <div><b>${total - running}</b><i>STOPPED</i></div>
        </div>
      </div>`;
  const guestCards = state.connections
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
          <button class="btn icon" data-dc-hwedit="${esc(vm.name)}" data-conn="${conn.id}" title="Edit Hardware">${ic.edit()}</button>
        </div>
      </div>`).join("");


  let body: string;
  if (state.dcTab === "dashboard") {
    body = `<div id="dc-dash">${skeleton("dashboard")}</div>`;
  } else if (state.dcTab === "containers") {
    // Same quick statistics as the Guests tab, over the tree's container cache;
    // each card names its own server, so its actions do too.
    const all = state.connections.flatMap((c) => (treeCts[c.id] ?? []).map((v) => ({ conn: c, ct: v })));
    const running = all.filter(({ ct }) => ct.active).length;
    body = `${quickStats(all.length, running, "CONTAINERS", ic.container)}
       <div class="section-title">Containers</div>
       ${all.length ? `<div class="card-grid">${all.map(({ conn, ct }) => ctCard(ct, conn.id, conn.name)).join("")}</div>`
         : emptyState("containers", "Create Container", "dc-ct-create")}`;
  } else if (state.dcTab === "networks") {
    const all = state.connections.flatMap((c) => (treeNets[c.id] ?? []).map((n) => ({ conn: c, n })));
    const active = all.filter(({ n }) => n.active).length;
    body = `${quickStats(all.length, active, "NETWORKS", ic.network)}
       <div class="section-title">Networks</div>
       ${all.length ? `<div class="card-grid">${all.map(({ conn, n }) => netCard(n, conn.id, conn.name)).join("")}</div>`
         : emptyState("networks", "Create Network", "dc-net-create")}`;
  } else if (state.dcTab === "storage") {
    const all = state.connections.flatMap((c) => (treePools[c.id] ?? []).map((p) => ({ conn: c, p })));
    const active = all.filter(({ p }) => p.active).length;
    body = `${quickStats(all.length, active, "POOLS", ic.storage)}
       <div class="section-title">Storage Pools</div>
       ${all.length ? `<div class="card-grid">${all.map(({ conn, p }) => poolCard(p, conn.id, { server: conn.name })).join("")}</div>`
         : emptyState("storage pools", "Create Pool", "dc-pool-create")}`;
  } else if (state.dcTab === "guests") {
    // Every guest across every server, so a VM is reachable directly from here
    // instead of only through the sidebar.
    body = `${quickStats(allVms.length, runningVms, "GUESTS", ic.vms)}<div class="section-title">Guests</div>
       ${guestCards ? `<div class="card-grid">${guestCards}</div>` : emptyState("guests", "Create Guest", "dc-guest-create")}`;
  } else {
    body = `<div id="users-area">${skeleton("table")}</div>`;
  }
  const restoreTabFocus = holdFocus(content, ".tab.active");
setHtml(content, `<div class="dhead"><span class="dhead-ic">${ic.dc(20)}</span><span class="dtitle">Datacenter</span></div>${tabs}${body}`);
  restoreTabFocus();

  if (state.dcTab === "dashboard") void loadDcDashboard();

  // Creating from the Datacenter is a Datacenter action: the form takes an
  // explicit connection and the view stays put, rather than switching the
  // whole app to the daemon the user picked.
  $("#dc-guest-create")?.addEventListener("click", () => void openCreateVm());
  $("#dc-ct-create")?.addEventListener("click", () => dcCreate((conn) => void openCreateCt(conn)));
  $("#dc-net-create")?.addEventListener("click", () => dcCreate((conn) => openCreateNetwork(conn)));
  $("#dc-pool-create")?.addEventListener("click", () => dcCreate((conn) => openCreatePool(conn)));
  bindCtCards(content);
  bindDcNetCards(content);
  bindDcPoolCards(content);
  content.querySelectorAll("[data-dctab]").forEach((b: Element) => {
    (b as HTMLElement).onclick = () => {
      state.dcTab = (b as HTMLElement).dataset.dctab as typeof state.dcTab;
      renderDc();
    };
  });
  /// Open a guest from the Datacenter cards, switching server if needed.
  const openVm = (connId: string, vm: string, tab: typeof state.vmTab) => {
    leaveCurrentView();
    state.selectedConn = connId;
    state.selectedVm = vm;
    state.ctMode = false;
    state.vmTab = tab;
    renderTree();
    void hooks.renderVm();
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
      // The editor reads through state.selectedConn, so point at the owning server.
      state.selectedConn = el.dataset.conn!;
      void openHardwareEditor({ vm: el.dataset.dcHwedit! });
    };
  });
}

/// A rendered Datacenter pool card's buttons. Each acts on the owned server
/// through `state.selectedConn`, so pointing it there is harmless (the card was not
/// in that server's page); Volumes opens the pool's page there, which is the
/// one action that must genuinely switch views.
function bindDcPoolCards(root: Element) {
  root.querySelectorAll<HTMLElement>("[data-pool-open]").forEach((card) => {
    card.onclick = (e) => {
      if ((e.target as Element).closest?.("button")) return;
      selectPoolRow(card.dataset.poolOpenConn!, card.dataset.poolOpen!);
    };
  });
  root.querySelectorAll<HTMLElement>("[data-vols]").forEach((b) => {
    b.onclick = (e) => {
      e.stopPropagation();
      selectPoolRow(b.dataset.volsConn!, b.dataset.vols!);
    };
  });
  root.querySelectorAll<HTMLElement>("[data-upload]").forEach((b) => {
    b.onclick = (e) => {
      e.stopPropagation();
      state.selectedConn = b.dataset.uploadConn!;
      renderTree();
      openUpload(b.dataset.upload!);
    };
  });
  root.querySelectorAll<HTMLElement>("[data-delpool]").forEach((b) => {
    b.onclick = async (e) => {
      e.stopPropagation();
      state.selectedConn = b.dataset.delpoolConn!;
      renderTree();
      await deletePool(b.dataset.delpool!);
      renderDc();
    };
  });
}

/// A rendered Datacenter network card's buttons: the same actions a server's
/// Networks tab binds, against the card's own server.
function bindDcNetCards(root: Element) {
  root.querySelectorAll<HTMLElement>("[data-net-open]").forEach((card) => {
    card.onclick = (e) => {
      if ((e.target as Element).closest?.("button")) return;
      selectNetRow(card.dataset.netConn!, card.dataset.netOpen!);
    };
  });
  root.querySelectorAll<HTMLElement>("[data-net-toggle]").forEach((b) => {
    b.onclick = async (e) => {
      e.stopPropagation();
      state.selectedConn = b.dataset.netConn!;
      renderTree();
      await netAction(b.dataset.netAction === "start" ? "start" : "stop", b.dataset.netToggle!);
      renderDc();
    };
  });
  root.querySelectorAll<HTMLElement>("[data-net-edit]").forEach((b) => {
    b.onclick = (e) => {
      e.stopPropagation();
      state.selectedConn = b.dataset.netConn!;
      renderTree();
      openEditNetwork(b.dataset.netEdit!);
    };
  });
  root.querySelectorAll<HTMLElement>("[data-delnet]").forEach((b) => {
    b.onclick = async (e) => {
      e.stopPropagation();
      state.selectedConn = b.dataset.delnetConn!;
      renderTree();
      await deleteNetwork(b.dataset.delnet!);
      renderDc();
    };
  });
}

/// Datacenter dashboard: aggregate every reachable server's host metrics into
/// one cluster view — total CPU, memory, disk, and network, plus per-server
/// rows that open that server's own dashboard. Same widgets as the daemon
/// dashboard, so the two reads are visually consistent.
export async function loadDcDashboard() {
  const body = $("#dc-dash");
  if (!body) return;

  const per = await Promise.all(
    state.connections.map(async (c) => {
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
  const vms = state.connections.flatMap((c) => treeVms[c.id] ?? []);
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
  const diskUsed = pools.reduce((a, { p }) => a + poolUsed(p), 0);
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
    <div class="cards">
      <div class="card"><span class="card-ic">${ic.server(20)}</span><div><b>${state.connections.length}</b><i>SERVERS</i></div></div>
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
          <div class="chart-axis">${rangeSelect()}<span>now</span></div>
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
          <div class="chart-axis">${rangeSelect()}<span>now</span></div>
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
          <div class="chart-axis">${rangeSelect()}<span>now</span></div>
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
        ${state.connections.map((c) => {
          const row = byId.get(c.id);
          if (!row) {
            return `<tr><td class="tname" style="cursor:pointer" tabindex="0" data-dc-dash-conn="${c.id}">${esc(c.name)} <span class="state bad">Offline</span></td><td colspan="3" class="muted">${esc(c.offline_error ?? "not answering")}</td></tr>`;
          }
          const { h, pools: ps } = row;
          const { cpuNow: sCpu } = addHostSample(`dc:${c.id}`, h);
          const mUsed = h.mem_total_kib - h.mem_available_kib;
          const mPct = h.mem_total_kib ? Math.max(0, Math.min(100, (mUsed / h.mem_total_kib) * 100)) : 0;
          const dTotal = ps.reduce((a, p) => a + (p.capacity ?? 0), 0);
          const dUsed = ps.reduce((a, p) => a + poolUsed(p), 0);
          const dPct = dTotal > 0 ? Math.max(0, Math.min(100, (dUsed / dTotal) * 100)) : 0;
          return `
          <tr>
            <td class="tname" style="cursor:pointer" tabindex="0" data-dc-dash-conn="${c.id}">${esc(c.name)}</td>
            <td>${pct(sCpu)}</td>
            <td>${mPct.toFixed(0)}% · ${fmtBytes(mUsed * 1024)}</td>
            <td>${dPct.toFixed(0)}% · ${fmtBytes(dUsed)}</td>
          </tr>`;
        }).join("") || `<tr><td colspan="4" class="empty">No servers.</td></tr>`}
      </table>
    </div>

    <div class="dash-section">
      <div class="section-title">Networking</div>
      <table class="grid">
        <tr><th>Name</th><th>Server</th><th>Subnet</th><th>State</th></tr>
        ${nets.map(({ c, n }) => `
          <tr class="row-open" tabindex="0" data-dash-net="${esc(n.name)}" data-dash-conn="${c.id}"><td>${esc(n.title || n.name)}</td><td>${esc(c.name)}</td><td>${n.ip ? esc(n.ip) : "—"}</td>
          <td><span class="state ${n.active ? "on" : ""}">${n.active ? "Active" : "Inactive"}</span></td></tr>`).join("") || `<tr><td colspan="4" class="empty">No networks.</td></tr>`}
      </table>
    </div>

    <div class="dash-section">
      <div class="section-title">Storage Pools</div>
      ${pools.map(({ c, p }) => {
        const used = poolUsed(p);
        const cap = p.capacity ?? 0;
        const usedPct = cap > 0 ? Math.max(0, Math.min(100, (used / cap) * 100)) : 0;
        return `
        <div class="storage-card ${p.active ? "ok" : ""}" data-open tabindex="0" data-dash-pool="${esc(p.name)}" data-dash-conn="${c.id}">
          <div class="storage-card-head">
            <span class="storage-card-icon">${ic.storage()}</span>
            <span class="storage-card-title">${esc(p.title || p.name)}<span class="host" style="display:block;font-weight:400">${esc(c.name)}</span></span>
              </div>
          <div class="metric-value">${usedPct.toFixed(usedPct >= 10 ? 0 : 1)}%</div>
          <div class="meter"><div class="meter-fill ${usedPct > 85 ? "hot" : ""}" style="width:${usedPct.toFixed(1)}%"></div></div>
          <div class="metric-foot">
            <span>${fmtBytes(used)} used</span>
            <span>${fmtBytes(cap)} total</span>
          </div>
        </div>`;
      }).join("") || `<div class="storage-card storage-card-none">
          <div class="storage-card-head">
            <span class="storage-card-icon">${ic.storage()}</span>
            <span class="storage-card-title">No storage pools</span>
          </div>
          <div class="metric-value">—</div>
          <div class="meter"></div>
          <div class="metric-foot"><span>nothing to show</span></div>
        </div>`}
    </div>
    `);

  bindDashOpeners(body);
  body.querySelectorAll("[data-dc-dash-conn]").forEach((r: Element) => {
    (r as HTMLElement).onclick = () => {
      state.selectedConn = (r as HTMLElement).dataset.dcDashConn!;
      state.selectedVm = null;
      state.connTab = "dashboard";
      renderTree();
      void hooks.renderConn();
    };
  });
}
