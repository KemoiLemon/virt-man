import { adminRowMenu } from "../admin";
import { ic } from "../icons";
import { attachLongPress } from "../touch";
import type { AdminSection } from "../route";
import { openContextMenu, type CtxItem } from "../ctxmenu";
import { esc } from "../xml";
import { state, treeCts, treeNets, treeVms } from "./state";
import { hooks } from "./hooks";
import { deleteConnection, deleteNetwork, deletePool, deleteVm, renameVm } from "./actions";
import { avatarHtml, openProfile } from "../pages/profile";
import { openFetch, openUpload } from "../pages/transfers";
import { $, setHtml } from "./dom";
import { openCreateVm } from "../forms/create-vm";
import { openConnectionDialog } from "../forms/connection-dialog";
import { openAddResource, openCreatePool, openCreateVolume } from "../forms/create-resource";
import { openCreateNetwork, openEditNetwork } from "../forms/network-form";
import { roleLabel } from "./boot";
import { closeAdmin, openAdmin } from "./admin-window";
import { canShareVm, downloadVv, openShareVm, refreshTreeVms, renderTree, selectCtRow, selectNetRow, selectPoolRow, selectServer, selectVmRow, treeCtPower, treeNetPower, treeVmPower } from "./tree";

// ------------------------------------------------------------- right-click

/// Actions for one container, on every surface it appears: the tree, the
/// container cards (a server's tab and the Datacenter's), the page header and
/// its tabs, and the palette's equivalents.
export function ctMenu(connId: string, name: string): CtxItem[] {
  const guest = (treeCts[connId] ?? []).find((v) => v.name === name);
  const running = guest?.active === true;
  const share = canShareVm(connId, name, true);
  return [
    { label: "Open", icon: ic.container(), run: () => selectCtRow(connId, name) },
    { label: "Start", icon: ic.start(), disabled: running, separator: true, run: () => void treeCtPower(connId, name, "/start") },
    { label: "Shutdown", icon: ic.stop(), disabled: !running, run: () => void treeCtPower(connId, name, "/shutdown") },
    { label: "Reboot", icon: ic.reboot(), disabled: !running, run: () => void treeCtPower(connId, name, "/reboot") },
    { label: "Force Stop", icon: ic.forceStop(), disabled: !running, run: () => void treeCtPower(connId, name, "/destroy") },
    { label: "Console", icon: ic.keyboard(), separator: true, run: () => selectCtRow(connId, name, "console") },
    { label: "Hardware", icon: ic.cpu(), run: () => selectCtRow(connId, name, "hardware") },
    { label: "Options", icon: ic.options(), run: () => selectCtRow(connId, name, "options") },
    { label: "Network", icon: ic.network(), run: () => selectCtRow(connId, name, "network") },
    { label: "XML", icon: ic.xml(), run: () => selectCtRow(connId, name, "xml") },
    ...(share ? [{ label: "Share", icon: ic.perms(), separator: true, run: () => void openShareVm(connId, name, true) }] : []),
    { label: "Rename", icon: ic.edit(), separator: !share, run: () => void hooks.renameCt(connId, name) },
    // A running container is stopped by the delete itself, as a VM is.
    { label: "Delete", icon: ic.trash(), danger: true, run: () => void hooks.deleteCtOn(connId, name, () => hooks.afterCtDelete(connId)) },
  ];
}

export function vmMenu(connId: string, vmName: string): CtxItem[] {
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
      label: "Force Stop", icon: ic.forceStop(), disabled: !running,
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
      run: () => { state.selectedConn = connId; void renameVm(vmName); },
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
export function netMenu(connId: string, netName: string): CtxItem[] {
  const net = (treeNets[connId] ?? []).find((n) => n.name === netName);
  const active = net?.active === true;
  // The editors read the selected server; point it at this network's owner
  // first, since a dashboard row can belong to a server that is not selected.
  const onConn = (f: () => void) => () => { state.selectedConn = connId; f(); };
  return [
    { label: "Open", icon: ic.network(), run: () => selectNetRow(connId, netName) },
    {
      label: active ? "Stop" : "Start", icon: active ? ic.stop() : ic.start(), separator: true,
      run: () => void treeNetPower(connId, netName, active ? "stop" : "start"),
    },
    { label: "Edit Network", icon: ic.edit(), separator: true, run: onConn(() => void openEditNetwork(netName)) },
    { label: "Firewall Rules", icon: ic.firewall(), run: onConn(() => hooks.openFirewallEditor(netName)) },
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
export function poolMenu(connId: string, poolName: string): CtxItem[] {
  const onConn = (f: () => void) => () => { state.selectedConn = connId; f(); };
  return [
    { label: "Open", icon: ic.storage(), run: () => selectPoolRow(connId, poolName) },
    { label: "Upload ISO", icon: ic.upload(), separator: true, run: onConn(() => openUpload(poolName)) },
    { label: "Download ISO from URL", icon: ic.download(), run: onConn(() => openFetch(poolName)) },
    { label: "New Volume", icon: ic.disk(), run: onConn(() => openCreateVolume(poolName)) },
    {
      label: "Delete Pool", icon: ic.trash(), danger: true, separator: true,
      run: () => {
        selectPoolRow(connId, poolName);
        void deletePool(poolName).then(() => refreshTreeVms());
      },
    },
  ];
}

/// Actions for one server (tree row, server card, dashboard row).
export function serverMenu(serverId: string): CtxItem[] {
  const create = (f: () => void) => () => { state.selectedConn = serverId; renderTree(); f(); };
  return [
    { label: "Dashboard", icon: ic.datacenter(), run: () => selectServer(serverId, "dashboard") },
    { label: "VMs", icon: ic.vms(), run: () => selectServer(serverId, "vms") },
    { label: "Containers", icon: ic.container(), run: () => selectServer(serverId, "containers") },
    { label: "Networks", icon: ic.network(), run: () => selectServer(serverId, "networks") },
    { label: "Storage", icon: ic.storage(), run: () => selectServer(serverId, "storage") },
    { label: "Firewall", icon: ic.firewall(), run: () => selectServer(serverId, "firewall") },
    { label: "Create VM", icon: ic.plus(), separator: true, run: create(() => openCreateVm()) },
    { label: "Create Container", icon: ic.plus(), run: create(() => void hooks.openCreateCt(serverId)) },
    { label: "Create Network", icon: ic.plus(), run: create(() => openCreateNetwork()) },
    { label: "Create Pool", icon: ic.plus(), run: create(() => openCreatePool()) },
    { label: "Edit Server", icon: ic.edit(), separator: true, run: () => openConnectionDialog(serverId) },
    { label: "Remove Server", icon: ic.trash(), danger: true, run: () => void deleteConnection(serverId) },
  ];
}

/// Creation and refresh: the sidebar background and the Datacenter root.
/// This is where Add Resource lives now that the sidebar button is gone.
export function rootMenu(): CtxItem[] {
  return [
    { label: "Add Resource", icon: ic.plus(), run: () => openAddResource() },
    { label: "Refresh", icon: ic.reboot(), separator: true, run: () => void refreshTreeVms() },
  ];
}

/// The actions offered for whatever tree row was right-clicked.
export function treeMenuItems(row: HTMLElement | null): CtxItem[] {
  if (!row || row.id === "dc-row") return rootMenu();
  const d = row.dataset;
  if (d.vm && d.vmConn) return vmMenu(d.vmConn, d.vm);
  if (d.ct && d.ctConn) return ctMenu(d.ctConn, d.ct);
  if (d.net && d.netConn) return netMenu(d.netConn, d.net);
  if (d.pool && d.poolConn) return poolMenu(d.poolConn, d.pool);
  if (d.conn) return serverMenu(d.conn);
  return rootMenu();
}

/// A right-clicked admin sidebar row: open it, and its section's main action.
/// Without this the admin sidebar offered the resource tree's menu (Add
/// server, Create VM), which belongs to a page the user has left.
export function adminTreeMenu(row: HTMLElement | null): CtxItem[] | null {
  const sec = row?.dataset.adminSection as AdminSection | undefined;
  if (!sec) return null;
  const open = (s: AdminSection) => () => openAdmin(s);
  // What each section's own add button does, reached from the menu. It opens
  // the section, then clicks its real button, so it is the same code path.
  const add: Partial<Record<AdminSection, [string, string]>> = {
    users: ["Add User", "#add-user"],
    groups: ["Add Group", "#add-group"],
    roles: ["New Role", "#add-role"],
    permissions: ["Add Permission", "#add-acl"],
    daemons: ["Add Daemon", "#add-daemon"],
    keys: ["New API Key", "#add-key"],
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
  items.push({ label: "Close", icon: ic.close(), separator: true, run: () => closeAdmin() });
  return items;
}

/// Menu for something right-clicked in the content pane, or null to leave the
/// browser's own menu alone. Only resource surfaces (cards, dashboard rows,
/// tabs) are claimed: text, the XML and log panes and every form field keep
/// the native menu, so copy and paste still work there.
export function contentMenuItems(target: Element): CtxItem[] | null {
  if (target.closest("input, textarea, select, .cmd, pre, .select-popover")) return null;
  const el = target.closest<HTMLElement>(
    "[data-vm-open], [data-dc-vm], [data-dc-ct], [data-net-open], [data-dash-net], [data-pool-open], " +
    "[data-dash-pool], [data-open-conn-row], [data-dc-dash-conn], .tab[data-tab], .tab[data-vmtab], " +
    ".tab[data-dctab], .dhead",
  );
  if (!el) return null;
  const d = el.dataset;
  if (d.vmOpen && state.selectedConn) return vmMenu(state.selectedConn, d.vmOpen);
  if (d.dcVm && d.dcVmConn) return vmMenu(d.dcVmConn, d.dcVm);
  if (d.dcCt && d.dcCtConn) return ctMenu(d.dcCtConn, d.dcCt);
  if (d.netOpen && state.selectedConn) return netMenu(state.selectedConn, d.netOpen);
  if (d.dashNet && d.dashConn) return netMenu(d.dashConn, d.dashNet);
  if (d.poolOpen && state.selectedConn) return poolMenu(state.selectedConn, d.poolOpen);
  if (d.dashPool && d.dashConn) return poolMenu(d.dashConn, d.dashPool);
  if (d.openConnRow) return serverMenu(d.openConnRow);
  if (d.dcDashConn) return serverMenu(d.dcDashConn);
  // A server tab: the server-level actions.
  if (d.tab && state.selectedConn && !state.selectedVm) {
    return serverMenu(state.selectedConn);
  }
  if (d.vmtab && state.selectedConn && state.selectedVm) {
    return state.ctMode ? ctMenu(state.selectedConn, state.selectedVm) : vmMenu(state.selectedConn, state.selectedVm);
  }
  if (d.dctab) return rootMenu();
  // The page header stands for the page's own resource.
  if (el.matches(".dhead")) {
    if (!state.selectedConn) return rootMenu();
    if (state.selectedVm) return state.ctMode ? ctMenu(state.selectedConn, state.selectedVm) : vmMenu(state.selectedConn, state.selectedVm);
    if (state.selectedNet) return netMenu(state.selectedConn, state.selectedNet);
    if (state.selectedPool) return poolMenu(state.selectedConn, state.selectedPool);
    return serverMenu(state.selectedConn);
  }
  return null;
}

/// Open the actions for whatever resource is under `target`, at viewport
/// coordinates. Returns whether anything was offered.
///
/// This is the whole of the right-click behaviour, and touch reuses it: a
/// long press on a phone has to open the SAME menu, built by the same `*Menu`
/// functions, or every action would need a second, touch-only definition.
export function menuAt(target: Element | null, x: number, y: number): boolean {
  if (!target?.closest) return false;
  let items: CtxItem[] | null = null;
  if (target.closest(".admin-win")) {
    // The admin window has its own actions: a section in its sidebar offers
    // open and its common action; a row offers exactly its own buttons.
    if (target.closest(".admin-nav")) items = adminTreeMenu(target.closest<HTMLElement>("[data-admin-section]"));
    else if (target.closest("#admin-area") && !target.closest("input, textarea, select, .cmd, pre, .select-popover")) {
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

export function renderProfileBtn() {
  const el = $("#profile-btn");
  if (!el || !state.user) return;
  el.onclick = openProfile;
  setHtml(el, `
    <div class="avatar">${avatarHtml()}</div>
    <div class="who"><b>${esc(state.user.nickname)}</b><span>${roleLabel(state.user.admin)}</span></div>`);
}
