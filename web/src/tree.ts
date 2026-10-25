import { api } from "./api";
import type { ContainerSummary, DomainSummary, NetworkSummary, PoolSummary } from "./api";
import { ic } from "./icons";
import { holdFocus } from "./focus";
import { esc } from "./xml";
import { state, treeCts, treeNets, treePools, treeVms } from "./state";
import { capLabel, displayName, nameOf } from "./util";
import { openModal, toast } from "./dialogs";
import { act } from "./actions";
import { hooks } from "./hooks";
import { clearResourceSelection, fastLoad, guestBase, guestList, leaveCurrentView, renderView, showPageSkeleton } from "./nav";
import { $, noteTreeSel, setHtml } from "./dom";
import { canAdmin } from "./boot";
import { syncAdminButton } from "./admin-window";

// ---------------------------------------------------------------- tree

/// Nested resource tree: Datacenter root -> servers -> guests, mirroring
/// vm-man's sidebar. Guests come from the per-connection cache, so opening a
/// server reveals its VMs inline instead of only in the content pane.
/// A row in the tree that stands where a group's rows would have been, saying
/// the group is empty. Same shape as a real row (icon slot, indent, label) so
/// the tree keeps its silhouette and a server with no pools still reads as
/// having a pool group -- but not a button, and not focusable: there is nothing
/// behind it to open. `cls` is the real row's own class, which carries the
/// indent for that kind.
export function ghostRow(cls: string, icon: string, label: string): string {
  return `
      <div class="tree-row tree-ghost ${cls}" aria-disabled="true">
        <span class="tree-ic">${icon}</span>
        <span class="tree-label">${esc(label)}</span>
      </div>`;
}

export function renderTree() {
  const tree = $("#tree");
  if (!tree) return;
  syncAdminButton();
  const serverRows = state.connections
    .map((c) => {
      const selected = c.id === state.selectedConn && !state.selectedVm && !state.selectedNet && !state.selectedPool;
      const vms = treeVms[c.id] ?? [];
      const cts = treeCts[c.id] ?? [];
      const nets = treeNets[c.id] ?? [];
      const pools = treePools[c.id] ?? [];
      const running = vms.filter((v) => v.active).length + cts.filter((v) => v.active).length;
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
      <button class="tree-row tree-vm ${!state.ctMode && state.selectedVm === v.name && state.selectedConn === c.id ? "sel" : ""}"
        data-vm="${esc(v.name)}" data-vm-conn="${c.id}">
        <span class="tree-ic ${v.active ? "on" : ""}">${ic.vm()}</span>
        <span class="tree-label">${esc(v.title || v.name)}</span>
        <span class="tree-state ${v.active ? "on" : ""}">${esc(capLabel(v.state))}</span>
      </button>`,
              )
              .join("")
          : "";
      // Containers sit below the VMs and above networks and pools: both are
      // guests, and the tree lists guests first.
      const ctRows = cts
        .map(
          (v) => `
      <button class="tree-row tree-vm tree-ct ${state.ctMode && state.selectedVm === v.name && state.selectedConn === c.id ? "sel" : ""}"
        data-ct="${esc(v.name)}" data-ct-conn="${c.id}">
        <span class="tree-ic ${v.active ? "on" : ""}">${ic.container()}</span>
        <span class="tree-label">${esc(v.title || v.name)}</span>
        ${v.privileged ? `<span class="tree-state bad" title="Privileged: its root is the host's root">priv</span>` : ""}
        <span class="tree-state ${v.active ? "on" : ""}">${esc(capLabel(v.state))}</span>
      </button>`,
        )
        .join("");
      const netRows = nets.length
        ? nets
            .map(
              (n) => `
      <button class="tree-row tree-res tree-net ${state.selectedNet === n.name && state.selectedConn === c.id ? "sel" : ""}"
        data-net="${esc(n.name)}" data-net-conn="${c.id}">
        <span class="tree-ic ${n.active ? "on" : ""}">${ic.network()}</span>
        <span class="tree-label">${esc(n.title || n.name)}</span>
        <span class="tree-state ${n.active ? "on" : ""}">${n.active ? "Active" : "Inactive"}</span>
      </button>`,
            )
            .join("")
        : ghostRow("tree-res tree-net", ic.network(), "No networks");
      const poolRows = pools.length
        ? pools
            .map(
              (p) => `
      <button class="tree-row tree-res tree-pool ${state.selectedPool === p.name && state.selectedConn === c.id ? "sel" : ""}"
        data-pool="${esc(p.name)}" data-pool-conn="${c.id}">
        <span class="tree-ic ${p.active ? "on" : ""}">${ic.storage()}</span>
        <span class="tree-label">${esc(p.title || p.name)}</span>
        <span class="tree-state ${p.active ? "on" : ""}">${p.active ? "Active" : "Inactive"}</span>
      </button>`,
            )
            .join("")
        : ghostRow("tree-res tree-pool", ic.storage(), "No storage pools");
      // Offline: the guest, network and pool states below are last known,
      // not current, so they are greyed rather than shown as live.
      return `
      <button class="tree-row tree-group ${selected ? "sel" : ""} ${offline ? "offline" : ""}" data-conn="${c.id}"
        ${offline ? `title="Offline${c.offline_error ? `: ${esc(c.offline_error)}` : ""}"` : ""}>
        <span class="tree-ic ${running > 0 && !offline ? "on" : ""}">${offline ? ic.offline() : ic.server()}</span>
        <span class="tree-label">${esc(c.name)}</span>
        <span class="tree-state ${offline ? "bad" : ""}">${offline ? "Offline" : ""}</span>
      </button>${offline ? `<div class="tree-stale">${guestRows}${ctRows}${netRows}${poolRows}</div>` : `${guestRows}${ctRows}${netRows}${poolRows}`}`;
    })
    .join("");

  const restoreTreeFocus = holdFocus(tree, ".tree-row.sel");
  setHtml(tree, `
    <button class="tree-row tree-root ${state.selectedConn === null ? "sel" : ""}" id="dc-row">
      <span class="tree-ic">${ic.dc()}</span>
      <span class="tree-label">Datacenter</span>
    </button>
    ${serverRows || ghostRow("tree-group", ic.server(), "No servers")}`);

  restoreTreeFocus();
  noteTreeSel(tree, false);
  $("#dc-row").onclick = () => {
    leaveCurrentView();
    state.selectedConn = null;
    clearResourceSelection();
    state.dcTab = "dashboard";
    renderTree();
    hooks.renderDc();
  };
  tree.querySelectorAll("[data-conn]").forEach((row: Element) => {
    (row as HTMLElement).onclick = () => {
      // Another server keeps the tab open on this one; coming from anywhere
      // else starts at the dashboard.
      const onServer = state.selectedConn !== null && !state.selectedVm && !state.selectedNet && !state.selectedPool;
      leaveCurrentView();
      state.selectedConn = (row as HTMLElement).dataset.conn!;
      clearResourceSelection();
      if (!onServer) state.connTab = "dashboard";
      renderTree();
      hooks.renderConn();
      void refreshTreeVms();
    };
  });
  tree.querySelectorAll("[data-vm]").forEach((row: Element) => {
    const el = row as HTMLElement;
    el.onclick = () => {
      // Set the connection first: renderVm() bails out when state.selectedConn is
      // null, which made guest clicks silently do nothing from the
      // Datacenter view or a different server's branch.
      // From another guest, stay on the same tab (Hardware stays Hardware).
      const fromGuest = state.selectedVm !== null;
      leaveCurrentView();
      state.selectedConn = el.dataset.vmConn!;
      clearResourceSelection();
      state.selectedVm = el.dataset.vm!;
      state.ctMode = false;
      if (!fromGuest) state.vmTab = "summary";
      renderTree();
      void hooks.renderVm();
    };
  });
  tree.querySelectorAll("[data-ct]").forEach((row: Element) => {
    const el = row as HTMLElement;
    el.onclick = () => selectCtRow(el.dataset.ctConn!, el.dataset.ct!);
  });
  tree.querySelectorAll("[data-net]").forEach((row: Element) => {
    const el = row as HTMLElement;
    el.onclick = () => {
      leaveCurrentView();
      state.selectedConn = el.dataset.netConn!;
      clearResourceSelection();
      state.selectedNet = el.dataset.net!;
      renderTree();
      showPageSkeleton();
      void fastLoad(() => hooks.renderNetPage());
    };
  });
  tree.querySelectorAll("[data-pool]").forEach((row: Element) => {
    const el = row as HTMLElement;
    el.onclick = () => {
      leaveCurrentView();
      state.selectedConn = el.dataset.poolConn!;
      clearResourceSelection();
      state.selectedPool = el.dataset.pool!;
      renderTree();
      showPageSkeleton();
      void hooks.renderPoolPage();
    };
  });
}

/// Select a server (optionally opening one of its tabs).
export function selectServer(id: string, tab?: typeof state.connTab) {
  leaveCurrentView();
  state.selectedConn = id;
  clearResourceSelection();
  if (tab) state.connTab = tab;
  renderTree();
  hooks.renderConn();
}

/// Select a guest (optionally opening one of its tabs).
export function selectVmRow(conn: string, vm: string, tab?: typeof state.vmTab) {
  leaveCurrentView();
  state.selectedConn = conn;
  state.selectedVm = vm;
  state.ctMode = false;
  state.selectedNet = null;
  state.selectedPool = null;
  if (tab) state.vmTab = tab;
  renderTree();
  void hooks.renderVm();
}

/// Select a container (optionally opening one of its tabs).
export function selectCtRow(conn: string, ct: string, tab?: typeof state.vmTab) {
  const keep = state.selectedVm !== null ? state.vmTab : "summary";
  leaveCurrentView();
  state.selectedConn = conn;
  clearResourceSelection();
  state.selectedVm = ct;
  state.ctMode = true;
  state.vmTab = tab ?? keep;
  renderTree();
  void hooks.renderVm();
}

/// A lifecycle POST for a container from anywhere it is listed (tree, cards,
/// palette), whether or not it is the one open.
export async function treeCtPower(conn: string, ct: string, path: string) {
  try {
    await act(`${guestBase(conn, ct, true)}${path}`, "POST");
  } catch {
    return; // act already toasted the daemon's reason
  }
  await refreshTreeVms();
  if (state.selectedConn === conn && state.selectedVm === ct && state.ctMode) void hooks.renderVm();
}

export function selectNetRow(conn: string, net: string) {
  leaveCurrentView();
  state.selectedConn = conn;
  clearResourceSelection();
  state.selectedNet = net;
  renderTree();
  showPageSkeleton();
  void fastLoad(() => hooks.renderNetPage());
}

export function selectPoolRow(conn: string, pool: string) {
  leaveCurrentView();
  state.selectedConn = conn;
  clearResourceSelection();
  state.selectedPool = pool;
  renderTree();
  showPageSkeleton();
  void hooks.renderPoolPage();
}

/// Run a lifecycle POST for a guest straight from the tree, without needing
/// that guest to be open: refresh the sidebar, and repaint the VM view when
/// the action was aimed at the guest already on screen.
export async function treeVmPower(conn: string, vm: string, path: string) {
  try {
    await act(`/proxy/${conn}/domains/${encodeURIComponent(vm)}${path}`, "POST");
  } catch {
    return; // act already toasted the daemon's reason
  }
  await refreshTreeVms();
  if (state.selectedConn === conn && state.selectedVm === vm) void hooks.renderVm();
}

export async function treeNetPower(conn: string, net: string, action: "start" | "stop") {
  try {
    await act(`/proxy/${conn}/networks/${encodeURIComponent(net)}/${action}`, "POST");
  } catch {
    return;
  }
  await refreshTreeVms();
  if (state.selectedConn === conn && state.selectedNet === net) void hooks.renderNetPage();
}

/// Actions for one guest. Shared by the tree, the VM cards and the datacenter
/// guest cards, so a guest offers the same menu wherever it is right-clicked.
/// Download a virt-viewer file for a running guest and hand it to the
/// browser. Fetched on click, never linked: the file names a relay the daemon
/// opens for one viewer and one minute, so a stale href would point nowhere.
export async function downloadVv(connId: string, vm: string) {
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
export function canShareVm(connId: string, vmName: string, ct = false): boolean {
  const guest = (guestList(connId, ct) ?? []).find((v) => v.name === vmName);
  // A privileged container is a root shell on the host: an admin's to share.
  if (ct && (guest as ContainerSummary | undefined)?.privileged) return canAdmin();
  return canAdmin() || (!!guest?.mine && !guest?.suspended);
}

/// Share one of your guests (a VM, or a container with `ct`) with other users,
/// at a fixed level.
export async function openShareVm(connId: string, vmName: string, ct = false) {
  const shown = displayName(guestList(connId, ct), vmName);
  let data: { shares: { user: string; username: string; role: string }[]; roles: string[] };
  let people: { id: string; username: string }[];
  try {
    const q = `conn=${encodeURIComponent(connId)}&vm=${encodeURIComponent(vmName)}&kind=${ct ? "ct" : "vm"}`;
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
    CTUser: "Use — console, start and stop",
    CTAdmin: "Manage — also change its resources, settings and network",
  };
  const el = openModal(
    `Share — ${esc(shown)}`,
    `<div class="section-title">Shared with</div>
     <table class="grid" id="sh-list">
       <tr><th>User</th><th>Access</th><th class="col-actions"></th></tr>
       ${data.shares.map((x) => `<tr><td class="tname">${esc(x.username)}</td><td>${esc(LEVEL[x.role] ?? x.role)}</td>
         <td class="col-actions"><button class="btn danger icon" data-unshare="${esc(x.user)}" title="Stop Sharing">${ic.trash()}</button></td></tr>`).join("")
         || `<tr><td colspan="3" class="empty">Not shared with anyone.</td></tr>`}
     </table>
     <div class="section-title">Add Someone</div>
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
  const body = { conn: connId, vm: vmName, kind: ct ? "ct" : "vm" };
  $("#sh-close", el).onclick = () => el.remove();
  $("#sh-add", el).onclick = async () => {
    try {
      await api.put("/shares", { ...body, user: $("#sh-user", el).value, role: $("#sh-role", el).value });
      toast(`${shown} shared`);
      el.remove();
      void openShareVm(connId, vmName, ct);
    } catch (e) { toast((e as Error).message, "bad"); }
  };
  el.querySelectorAll<HTMLElement>("[data-unshare]").forEach((b) => {
    b.onclick = async () => {
      try {
        await api.del("/shares", { ...body, user: b.dataset.unshare });
        toast("Sharing stopped");
        el.remove();
        void openShareVm(connId, vmName, ct);
      } catch (e) { toast((e as Error).message, "bad"); }
    };
  });
}

/// Refresh the guest list backing the tree for the selected server.
export async function refreshTreeVms() {
  // Every server's guests, networks and pools, not just the selected one:
  // the tree auto-expands all three, so an unselected server still has to
  // show its resources.
  const results = await Promise.all(
    state.connections.map(async (c) => {
      try {
        const [vms, nets, pools, cts] = await Promise.all([
          api.get<DomainSummary[]>(`/proxy/${c.id}/domains`),
          api.get<NetworkSummary[]>(`/proxy/${c.id}/networks`),
          api.get<PoolSummary[]>(`/proxy/${c.id}/pools`),
          // A daemon without the container runtime, or no right to list
          // them: no containers, which must not take the VMs down with it.
          api.get<ContainerSummary[]>(`/proxy/${c.id}/containers`).catch(() => [] as ContainerSummary[]),
        ]);
        return [c.id, vms, nets, pools, cts] as const;
      } catch {
        // Unreachable daemon: keep whatever we last knew rather than
        // collapsing its branch. The content pane surfaces the error.
        return [c.id, treeVms[c.id] ?? [], treeNets[c.id] ?? [], treePools[c.id] ?? [], treeCts[c.id] ?? []] as const;
      }
    }),
  );

  // The sidebar lists whatever the server returns, which is already filtered
  // to what the caller may see: the proxy drops every guest, network and pool
  // the caller cannot audit, and a user holding a daemon-wide role (an
  // administrator, or a `Sys.Audit`/`VM.Audit` grant on the daemon) passes
  // those checks and so sees the daemon's whole inventory. Filtering again here
  // buried an administrator's own guests under nothing and hid the resources
  // they are meant to look after, so it is deliberately not done.
  let changed = false;
  for (const [id, vms, nets, pools, cts] of results) {
    if (JSON.stringify(treeVms[id]) !== JSON.stringify(vms)) {
      treeVms[id] = vms;
      changed = true;
    }
    if (JSON.stringify(treeCts[id]) !== JSON.stringify(cts)) {
      treeCts[id] = cts;
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
  // A link opened before the lists loaded holds a UUID where the selection
  // wants a name; now that the lists are here, swap it for the name.
  const sc = state.selectedConn;
  if (sc) {
    const was = [state.selectedVm, state.selectedNet, state.selectedPool];
    if (state.selectedVm) state.selectedVm = nameOf(guestList(sc), state.selectedVm);
    if (state.selectedNet) state.selectedNet = nameOf(treeNets[sc], state.selectedNet);
    if (state.selectedPool) state.selectedPool = nameOf(treePools[sc], state.selectedPool);
    if (was.some((x, i) => x !== [state.selectedVm, state.selectedNet, state.selectedPool][i])) {
      changed = true;
      renderView();
    }
  }
  if (changed) {
    hooks.syncVmActions();
    renderTree();
    // The Datacenter view lists the same guests, so keep it in step; without
    // this its Guests table stays empty until another action redraws it.
    // Its dashboard repaints itself in place from the same tree: rebuilding the
    // pane would paint the skeleton over the user's scroll position every time
    // a pool's allocation ticks, which is what made a poll reset the page.
    if (!state.selectedConn && !state.selectedVm) {
      if (state.dcTab === "dashboard") void hooks.loadDcDashboard();
      else hooks.renderDc();
    }
    // Network/pool pages poll their own data; the tree refresh above is what
    // keeps the sidebar counts and states live.
  }
}
