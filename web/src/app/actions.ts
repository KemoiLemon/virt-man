// Delete and rename actions for guests, networks, pools, volumes and servers.
// Each `*On` takes the connection explicitly so the admin panel can reuse the
// same function as the resource's own view; the views they repaint afterwards
// are reached through `hooks`.
import { api } from "../api";
import type { AdminResource, Connection } from "../api";
import { askConfirm, askText, toast } from "./dialogs";
import { hooks } from "./hooks";
import { guestList, renderView } from "./nav";
import { runTask } from "../task";
import { shownNet, shownPool, shownVolume, state, volumeNames } from "./state";
import { displayName, isUuid } from "./util";

export async function act(url: string, method: "POST" | "DELETE" = "POST") {
  try {
    const res = method === "POST" ? await api.post(url) : await api.del(url);
    toast("OK");
    return res;
  } catch (e) {
    toast((e as Error).message, "bad");
    throw e;
  }
}

/// Result of `DELETE /domains/{name}`: which backing stores went, which were
/// deliberately kept (with a reason), and which could not be removed. `stopped`
/// says the daemon had to shut a running guest down before it could be removed.
export interface DomainDeleteResult {
  stopped?: boolean;
  removed?: string[];
  kept?: { disk: string; reason: string }[];
  failed?: { disk: string; error: string }[];
}

/// Plain-language reason for a disk the daemon kept, so the user is never left
/// wondering why a "delete the disks" request left something behind.
function keepReasonText(reason: string): string {
  if (reason === "iso") return "ISO";
  if (reason === "readonly") return "read-only";
  if (reason === "in_use") return "in use by another guest";
  return reason;
}

/// Turn a delete result into the lines to report. Shared by the guest view and
/// the admin panel, so a partial cleanup reads the same wherever it was
/// started. `stopped` is named because "it was running and I stopped it" is
/// something the user should know happened.
export function deleteOutcomeLines(res: DomainDeleteResult | undefined): { lines: string[]; bad: boolean } {
  const removed = res?.removed ?? [];
  const kept = res?.kept ?? [];
  const failed = res?.failed ?? [];
  // Report the whole outcome, not just "Deleted": a partial cleanup that
  // looks complete is how a stale disk gets forgotten about.
  const lines: string[] = [];
  if (res?.stopped) lines.push("Stopped the running guest first");
  if (removed.length) lines.push(`Deleted ${removed.length} disk(s)`);
  if (kept.length) {
    lines.push(`Kept ${kept.map((k) => `${k.disk} (${keepReasonText(k.reason)})`).join(", ")}`);
  }
  if (failed.length) lines.push(`Could not delete ${failed.map((f) => f.disk).join(", ")}`);
  return { lines, bad: failed.length > 0 };
}

/// Delete a VM on `conn`. Shared by the guest view (`state.selectedConn`) and the
/// admin panel, so the two cannot drift apart — including stopping a running
/// guest first, which the daemon does.
export async function deleteVmOn(conn: string, name: string, after: () => void) {
  const shown = displayName(guestList(conn), name);
  if (!(await askConfirm("Delete VM", `Delete VM "${shown}"?`, "Delete", true))) return;
  const removeStorage = await askConfirm(
    "Remove Storage",
    // Say what will NOT go: an ISO the guest boots from is shared media, and a
    // disk another guest uses is not this VM's to delete.
    `Also delete the disks belonging to "${shown}"? ISOs and disks other guests use are kept.`,
    "Delete disks",
    true,
  );
  // A running guest is stopped before it is removed, which can take a few
  // seconds, so this runs as a task with its own progress rather than leaving
  // the button looking hung. The full outcome -- stopped, removed, kept and
  // why -- becomes the success message, or a partial disk cleanup would look
  // like a clean delete.
  let outcome = `VM ${shown} deleted`;
  await runTask(`Delete VM ${shown}`,  async (set) => {
    set("Stopping the guest if it is running…");
    const res = await api.del<DomainDeleteResult>(
      `/proxy/${conn}/domains/${encodeURIComponent(name)}?remove_storage=${removeStorage}`);
    if (state.selectedConn === conn && state.selectedVm === name) state.selectedVm = null;
    const lines = deleteOutcomeLines(res).lines;
    if (lines.length) outcome = `VM ${shown} deleted\n${lines.join("\n")}`;
  }, { done: "", notify: toast }).then((ok) => {
    if (ok) toast(outcome);
  });
  after();
  // The kept disks are still on the host, so keep the sidebar counts honest.
  void hooks.refreshTreeVms();
}

export async function deleteVm(name: string) {
  if (!state.selectedConn) return;
  await deleteVmOn(state.selectedConn, name, () => hooks.renderConn());
}

/// Rename a domain. libvirt can only rename shut-off persistent guests, so
/// the daemon rejects a running one and the caller surfaces that reason.
export async function renameVm(name: string) {
  if (!state.selectedConn) return;
  const byUuid = isUuid(name);
  const next = await askText("Rename VM", "New name", displayName(guestList(state.selectedConn), name), "alpine-worker");
  if (!next || next === name) return;
  try {
    await api.post(`/proxy/${state.selectedConn}/domains/${encodeURIComponent(name)}/rename`, { name: next });
    toast("Renamed");
    // A UUID-named guest is renamed by its title; its id (the URL and every
    // stored key) never changes. An older name-keyed guest swaps the name.
    if (!byUuid && state.selectedVm === name) state.selectedVm = next;
    await hooks.refreshTreeVms();
    if (state.selectedVm) hooks.renderVm();
    else hooks.renderConn();
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}

export async function deleteNetworkOn(conn: string, name: string, shown: string, after: () => void) {
  if (!(await askConfirm("Delete Network", `Delete network "${shown}"?`, "Delete", true))) return;
  try {
    await api.del(`/proxy/${conn}/networks/${encodeURIComponent(name)}`);
    toast("Deleted");
    after();
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}

export async function deleteNetwork(name: string) {
  if (!state.selectedConn) return;
  await deleteNetworkOn(state.selectedConn, name, shownNet(name), () => hooks.loadNetworks());
}

export async function deletePoolOn(conn: string, name: string, shown: string, after: () => void) {
  if (!(await askConfirm("Delete Pool", `Delete storage pool "${shown}"?`, "Delete", true))) return;
  try {
    await api.del(`/proxy/${conn}/pools/${encodeURIComponent(name)}`);
    toast("Deleted");
    after();
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}

export async function deletePool(name: string) {
  if (!state.selectedConn) return;
  await deletePoolOn(state.selectedConn, name, shownPool(name), () => hooks.loadPools());
}

export async function deleteVolumeOn(conn: string, pool: string, name: string, after: () => void) {
  if (!(await askConfirm("Delete Volume", `Delete volume "${shownVolume(pool, name)}"?`, "Delete", true))) return;
  try {
    await api.del(`/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes/${encodeURIComponent(name)}`);
    toast("Deleted");
    after();
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}

export function setVolumeTitle(conn: string, pool: string, name: string, title: string) {
  return api.put(
    `/proxy/${conn}/pools/${encodeURIComponent(pool)}/volumes/${encodeURIComponent(name)}/title`, { title });
}

export async function renameVolume(pool: string, name: string) {
  if (!state.selectedConn) return;
  const title = await askText("Volume Nickname", "Nickname", shownVolume(pool, name) === name ? "" : shownVolume(pool, name));
  if (title == null) return;
  try {
    await setVolumeTitle(state.selectedConn, pool, name, title);
    volumeNames[`${pool}/${name}`] = title.trim() || name;
    toast("Volume nickname saved");
  } catch (e) { toast((e as Error).message, "bad"); }
}

export async function deleteVolume(pool: string, name: string) {
  if (!state.selectedConn) return;
  await deleteVolumeOn(state.selectedConn, pool, name, () => hooks.loadVolumes(pool));
}

/// Delete one resource from the admin panel. It routes to the same delete the
/// resource's own view uses — including starting a running guest's stop — with
/// the connection named explicitly and `after` repainting the admin page
/// instead of navigating to the resource.
export async function adminDeleteResource(kind: AdminResource["kind"], conn: string, pool: string, name: string, after: () => void) {
  const shown = kind === "volume" ? shownVolume(pool, name) : name;
  if (kind === "vm") await deleteVmOn(conn, name, after);
  else if (kind === "ct") await hooks.deleteCtOn(conn, name, after);
  else if (kind === "network") await deleteNetworkOn(conn, name, shown, after);
  else if (kind === "pool") await deletePoolOn(conn, name, shown, after);
  else await deleteVolumeOn(conn, pool, name, after);
}

export async function deleteConnection(id: string) {
  const c = state.connections.find((x) => x.id === id);
  if (!c) return;
  if (!(await askConfirm("Remove Server", `Remove server "${c.name}"?`, "Remove", true))) return;
  try {
    await api.del(`/connections/${id}`);
    state.connections = await api.get<Connection[]>("/connections");
    if (state.selectedConn === id) state.selectedConn = null;
    hooks.renderTree();
    // Whatever is on screen: removing a daemon from the admin page stays there.
    renderView();
  } catch (e) {
    toast((e as Error).message, "bad");
  }
}
