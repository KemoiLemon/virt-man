import { openTayne, type TayneCommand } from "../tayne";
import { ic } from "../icons";
import { state, treeCts, treeNets, treePools, treeVms } from "./state";
import { displayName } from "./util";
import { hooks } from "./hooks";
import { clearResourceSelection, leaveCurrentView } from "./nav";
import { toggleTheme } from "./dom";
import { openUpload } from "../pages/transfers";
import { openCreateNetwork, openEditNetwork } from "../forms/network-form";
import { openCreateVm } from "../forms/create-vm";
import { openAddResource, openCreatePool, openCreateVolume } from "../forms/create-resource";
import { canAdmin, logout } from "./boot";
import { openAdmin } from "./admin-window";
import { deleteNetwork, deleteVm, renameVm } from "./actions";
import { downloadVv, renderTree, selectCtRow, selectNetRow, selectPoolRow, selectServer, selectVmRow, treeCtPower, treeVmPower } from "./tree";

// ------------------------------------------------------- command palette

/// Build the command list for the current context. Called on every open so the
/// palette always reflects live state (which server is selected, which guests
/// exist) rather than a snapshot taken at boot.
export function openCommandPalette() {
  const cmds: TayneCommand[] = [];
  const add = (id: string, label: string, hint: string | undefined, icon: string, run: () => void) =>
    cmds.push({ id, label, hint, icon, run });
  const toDc = (tab: typeof state.dcTab) => () => {
    leaveCurrentView();
    state.selectedConn = null;
    clearResourceSelection();
    state.dcTab = tab;
    renderTree();
    hooks.renderDc();
  };

  // Actions on what is open come first: they are what you most likely want.
  if (state.selectedConn && state.selectedVm && state.ctMode) {
    const conn = state.selectedConn;
    const ct = state.selectedVm;
    const shown = displayName(treeCts[conn], ct);
    const running = (treeCts[conn] ?? []).find((v) => v.name === ct)?.active === true;
    const power = (id: string, label: string, icon: string, path: string) =>
      add(id, `${label} — ${shown}`, "This container", icon, () => void treeCtPower(conn, ct, path));
    if (running) {
      power("ct-shutdown", "Shutdown", ic.stop(), "/shutdown");
      power("ct-reboot", "Reboot", ic.reboot(), "/reboot");
      power("ct-destroy", "Force Stop", ic.forceStop(), "/destroy");
    } else {
      power("ct-start", "Start", ic.start(), "/start");
    }
    add("ct-rename", `Rename — ${shown}`, "This container", ic.edit(), () => void hooks.renameCt(conn, ct));
    add("ct-delete", `Delete — ${shown}`, "This container", ic.trash(), () => void hooks.deleteCt(ct));
  }
  if (state.selectedConn && state.selectedVm && !state.ctMode) {
    const conn = state.selectedConn;
    const vm = state.selectedVm;
    const running = (treeVms[conn] ?? []).find((v) => v.name === vm)?.active === true;
    const power = (id: string, label: string, icon: string, path: string) =>
      add(id, `${label} — ${vm}`, "This VM", icon, () => void treeVmPower(conn, vm, path));
    if (running) {
      add("vm-vv", `Open in virt-viewer — ${vm}`, "This VM", ic.download(), () => void downloadVv(conn, vm));
      power("vm-shutdown", "Shutdown", ic.stop(), "/shutdown");
      power("vm-reboot", "Reboot", ic.reboot(), "/reboot");
      power("vm-reset", "Reset", ic.reset(), "/reset");
      power("vm-destroy", "Force Stop", ic.forceStop(), "/destroy");
    } else {
      power("vm-start", "Start", ic.start(), "/start");
      // libvirt can only rename a shut-off guest.
      add("vm-rename", `Rename — ${vm}`, "This VM", ic.edit(), () => void renameVm(vm));
    }
    add("vm-delete", `Delete — ${vm}`, "This VM", ic.trash(), () => void deleteVm(vm));
  }
  if (state.selectedConn && state.selectedNet) {
    const net = state.selectedNet;
    add("net-edit", `Edit Network — ${net}`, "This network", ic.edit(), () => void openEditNetwork(net));
    add("net-fw", `Firewall rules — ${net}`, "This network", ic.firewall(), () => hooks.openFirewallEditor(net));
    add("net-delete", `Delete network — ${net}`, "This network", ic.trash(),
      () => void deleteNetwork(net).then(() => hooks.refreshTreeVms()));
  }
  if (state.selectedConn && state.selectedPool) {
    const pool = state.selectedPool;
    add("pool-upload", `Upload ISO — ${pool}`, "This pool", ic.upload(), () => openUpload(pool));
    add("pool-volume", `New Volume — ${pool}`, "This pool", ic.disk(), () => openCreateVolume(pool));
    add("pool-templates", `Templates — ${pool}`, "This pool", ic.container(), () => void hooks.openTemplates(pool));
  }

  // Creation. Resource kinds that need a server are offered for the one open.
  add("add-resource", "Add resource", "VM, container, network or pool", ic.plus(), () => openAddResource());
  if (state.selectedConn) {
    const server = state.connections.find((c) => c.id === state.selectedConn)?.name ?? "";
    add("create-vm", "Create VM", server, ic.vm(), () => openCreateVm());
    add("create-ct", "Create Container", server, ic.container(), () => void hooks.openCreateCt());
    add("create-net", "Create Network", server, ic.network(), () => openCreateNetwork());
    add("create-pool", "Create Pool", server, ic.storage(), () => openCreatePool());
  }

  // Navigation. Every page with a URL is reachable, and the hint says where
  // it lives, so typing a server, guest, network or pool name finds it.
  add("nav-dc", "Datacenter", "Dashboard", ic.dc(), toDc("dashboard"));
  add("nav-dc-guests", "Datacenter — Guests", "/guests", ic.vms(), toDc("guests"));
  add("nav-dc-cts", "Datacenter — Containers", "/containers", ic.container(), toDc("containers"));
  const connTabs: [typeof state.connTab, string, string][] = [
    ["dashboard", "Dashboard", ic.datacenter()],
    ["vms", "VMs", ic.vms()],
    ["containers", "Containers", ic.container()],
    ["networks", "Networks", ic.network()],
    ["storage", "Storage", ic.storage()],
    ["firewall", "Firewall", ic.firewall()],
    ["shell", "Shell", ic.terminal()],
  ];
  const vmTabs: [typeof state.vmTab, string, string][] = [
    ["summary", "Summary", ic.vm()],
    ["console", "Console", ic.vm()],
    ["hardware", "Hardware", ic.cpu()],
    ["options", "Options", ic.options()],
    ["network", "Network", ic.network()],
    ["xml", "XML", ic.xml()],
  ];
  for (const c of state.connections) {
    for (const [tab, label, icon] of connTabs) {
      // The shell is an administrator's tool; the tab is hidden for anyone
      // else, and the daemon refuses it regardless.
      if (tab === "shell" && !canAdmin()) continue;
      add(`nav-${c.id}-${tab}`, `${c.name} — ${label}`, "Server", icon, () => selectServer(c.id, tab));
    }
    for (const v of treeVms[c.id] ?? []) {
      const name = v.title || v.name;
      for (const [tab, label, icon] of vmTabs) {
        add(`nav-${c.id}-vm-${v.name}-${tab}`, tab === "summary" ? `VM — ${name}` : `VM — ${name} — ${label}`,
          c.name, icon, () => selectVmRow(c.id, v.name, tab));
      }
    }
    for (const v of treeCts[c.id] ?? []) {
      const name = v.title || v.name;
      for (const [tab, label, icon] of vmTabs) {
        add(`nav-${c.id}-ct-${v.name}-${tab}`, tab === "summary" ? `Container — ${name}` : `Container — ${name} — ${label}`,
          c.name, tab === "summary" || tab === "console" ? ic.container() : icon, () => selectCtRow(c.id, v.name, tab));
      }
    }
    for (const n of treeNets[c.id] ?? []) {
      add(`nav-${c.id}-net-${n.name}`, `Network — ${n.title || n.name}`, c.name, ic.network(), () => selectNetRow(c.id, n.name));
    }
    for (const pl of treePools[c.id] ?? []) {
      add(`nav-${c.id}-pool-${pl.name}`, `Pool — ${pl.title || pl.name}`, c.name, ic.storage(), () => selectPoolRow(c.id, pl.name));
    }
  }

  add(
    "toggle-theme",
    "Toggle Theme",
    document.documentElement.getAttribute("data-theme") === "dark" ? "dark → light" : "light → dark",
    ic.theme(),
    toggleTheme,
  );
  if (canAdmin()) {
    add("admin-panel", "Administration", "Users, groups, permissions, resources, limits", ic.admin(), () => openAdmin());
    for (const [sec, label] of [["users", "Users"], ["groups", "Groups"], ["permissions", "Permissions"],
      ["vms", "VMs"], ["containers", "Containers"], ["networks", "Networks"], ["pools", "Storage Pools"],
      ["quotas", "Quotas"], ["ips", "IP Addresses"], ["daemons", "Daemons"],
      ["activity", "Activity"]] as const) {
      add(`admin-${sec}`, `Administration — ${label}`, `/admin/${sec}`, ic.admin(), () => openAdmin(sec));
    }
  }
  if (!state.authDisabled) add("logout", "Sign Out", "End this session", ic.logout(), () => void logout());

  openTayne(cmds);
}
