import { hooks } from "./hooks";
import { state } from "./state";
import { clearResourceSelection } from "./nav";
import { toast } from "./dialogs";
import { renderTree, selectNetRow, selectPoolRow } from "./tree";

/// A network's IPAM, from the daemon (`GET /networks/{name}/ipam`).
export interface IpamInfo {
  serving: boolean; gateway: string; prefix: number;
  range_start: string; range_end: string; range_size: number; range_used: number;
  dns_forwarders: string[]; domain: string | null; lease_secs: number;
}
/// One reservation (`GET /networks/{name}/hosts`).
export interface IpamHost {
  mac: string; ip: string; name: string | null; dynamic: boolean;
  vm: string | null; vm_name: string | null; seen_ms: number;
}

/// Return to the selected server's dashboard from a network/pool page.
export function backToConn() {
  state.selectedNet = null;
  state.selectedPool = null;
  state.connTab = "dashboard";
  renderTree();
  hooks.renderConn();
}

/// A network or pool page whose resource is gone (a stale link, or deleted
/// elsewhere) falls back to the server's list of that kind, replacing the dead
/// URL rather than stacking it in history, instead of a raw libvirt error.
export function leaveMissing(tab: "networks" | "storage", message: string) {
  clearResourceSelection();
  state.connTab = tab;
  state.restoringRoute = true;
  try {
    renderTree();
    hooks.renderConn();
  } finally {
    state.restoringRoute = false;
  }
  toast(message, "bad");
}

/// Dashboard pool cards and network rows open their management page. Shared
/// by the datacenter and server dashboards, which render the same widgets.
export function bindDashOpeners(root: HTMLElement) {
  root.querySelectorAll("[data-dash-pool]").forEach((el: Element) => {
    const d = (el as HTMLElement).dataset;
    (el as HTMLElement).onclick = () => selectPoolRow(d.dashConn!, d.dashPool!);
  });
  root.querySelectorAll("[data-dash-net]").forEach((el: Element) => {
    const d = (el as HTMLElement).dataset;
    (el as HTMLElement).onclick = () => selectNetRow(d.dashConn!, d.dashNet!);
  });
}
